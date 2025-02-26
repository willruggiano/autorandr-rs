use log::{error, info};
use x11rb::{
    connect,
    connection::Connection,
    cookie::Cookie,
    protocol::randr::{
        ConnectionExt as RandrExt, Crtc, GetCrtcInfoReply, GetOutputInfoReply,
        GetScreenResourcesCurrentReply, NotifyMask, Output, SetConfig, SetCrtcConfigReply,
        SetCrtcConfigRequest,
    },
    protocol::xproto::{Atom, ConnectionExt as XprotoExt, Timestamp, Window},
    protocol::Event,
};

use std::collections::{HashMap, HashSet};

use clap::ArgMatches;
use miette::{IntoDiagnostic, Result};
use thiserror::Error;

use crate::config::{Config, Mode, MonConfig, Position, SingleConfig};
use crate::{edid_atom, get_monitors, get_outputs, ok_or_exit};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Mode {0} not found")]
    ModeNotFound(Mode),
    #[error("Mode {0} not supported")]
    ModeNotSupported(Mode),
    #[error("No Crtc available for monitor {0}")]
    NoCrtc(String),
}

/// Find the config that matches the attached monitors. On a match, this returns a tuple of
/// (name, frame buffer size, map from output to output config).
fn get_config<'a, C: Connection>(
    config: &'a Config,
    conn: &'a C,
    outputs: &'a Vec<Output>,
    atom_edid: Atom,
) -> Option<(&'a String, &'a Mode, HashMap<Output, &'a MonConfig>)> {
    let out_to_mon: HashMap<_, _> = get_monitors(conn, outputs, atom_edid).collect();
    let mut monitors: Vec<_> = out_to_mon.values().cloned().collect();
    monitors.sort();
    let SingleConfig {
        name,
        setup,
        fb_size,
    } = config.0.get(&monitors)?;
    let mut out = HashMap::with_capacity(setup.len());
    for (output, mon) in out_to_mon.into_iter() {
        if let Some(moncfg) = setup.get(&mon) {
            out.insert(output, moncfg);
        }
    }
    Some((name, fb_size, out))
}

/// Create a map from human mode descriptions, in width and height, to Xorg mode identifiers
fn mode_map<C: Connection>(
    conn: &C,
    root: Window,
) -> Result<(HashMap<Mode, HashSet<u32>>, Timestamp)> {
    let resources = conn
        .randr_get_screen_resources(root)
        .into_diagnostic()?
        .reply()
        .into_diagnostic()?;
    let mut modes: HashMap<_, HashSet<u32>> = HashMap::with_capacity(resources.modes.len());
    for mi in resources.modes.iter() {
        modes
            .entry(Mode {
                w: mi.width,
                h: mi.height,
            })
            .or_default()
            .insert(mi.id);
    }
    Ok((modes, resources.timestamp))
}

/// Create a request to disable a CRTC or a default CRTC config request.
fn disable_crtc<'a, 'b>(crtc: u32, from: &'a GetCrtcInfoReply) -> SetCrtcConfigRequest<'b> {
    SetCrtcConfigRequest {
        crtc,
        timestamp: from.timestamp,
        config_timestamp: from.timestamp,
        x: from.x,
        y: from.y,
        mode: 0,
        rotation: from.rotation,
        outputs: Vec::new().into(),
    }
}

/// Allocate a CRTC for use by an output.
fn allocate_crtc(info: &GetOutputInfoReply, free: &mut HashSet<&Crtc>) -> Option<Crtc> {
    let dest = if info.crtc != 0 {
        Some(info.crtc)
    } else {
        info.crtcs.iter().find_map(|c| free.get(&c).map(|&&a| a))
    };
    if let Some(dest) = &dest {
        free.remove(dest);
    }
    dest
}

/// Find a matching mode id for the output within the mode map.
///
/// Since this is a helper function that's part of a command line utility,
/// errors are returned as strings
fn find_mode_id(
    info: &GetOutputInfoReply,
    mode_map: &HashMap<Mode, HashSet<u32>>,
    mode: &Mode,
) -> Result<u32> {
    let mode_ids = mode_map
        .get(&mode)
        .ok_or_else(|| Error::ModeNotFound(mode.clone()))
        .into_diagnostic()?;
    info.modes
        .iter()
        .find_map(|m| mode_ids.get(m).map(|&m| m))
        .ok_or_else(|| Error::ModeNotSupported(mode.clone()))
        .into_diagnostic()
}

/// Apply a batch of SetCrtcConfig commands.
fn batch_config<C: Connection>(conn: &C, batch: Vec<SetCrtcConfigRequest>) -> Result<()> {
    for req in &batch {
        if req.mode != 0 {
            info!(
                "Configuring CRTC {} to mode {} at {},{}",
                req.crtc, req.mode, req.x, req.y,
            );
        } else {
            info!("Disabling CRTC {}", req.crtc);
        }
    }
    info!("Batch pre-sent");
    let cookies: Vec<Cookie<C, SetCrtcConfigReply>> = batch
        .into_iter()
        .map(|req| req.send(conn))
        .collect::<std::result::Result<_, _>>()
        .into_diagnostic()?;
    info!("Batch sent");
    let responses: Vec<SetCrtcConfigReply> = cookies
        .into_iter()
        .map(|cookie| cookie.reply())
        .collect::<std::result::Result<_, _>>()
        .into_diagnostic()?;
    info!("Batch recieved");
    for (num, res) in responses.iter().enumerate() {
        match res.status {
            SetConfig::INVALID_CONFIG_TIME => {
                error!("Request #{} failed with invalid config time", num)
            }
            SetConfig::INVALID_TIME => error!("Request #{} failed with invalid time", num),
            SetConfig::FAILED => error!("Request #{} failed", num),
            _ => (),
        }
    }
    Ok(())
}

/// Make the current Xorg server match the specified configuration.
fn apply_config<C: Connection>(
    conn: &C,
    res: &GetScreenResourcesCurrentReply,
    fb_size: &Mode,
    setup: HashMap<Output, &MonConfig>,
    root: Window,
) -> Result<bool> {
    let (modes, timestamp) = mode_map(conn, root)?;
    let mut free_crtcs: HashSet<_> = res.crtcs.iter().collect();
    let mut enables = Vec::with_capacity(res.crtcs.len());
    let mut mm_w = 0;
    let mut mm_h = 0;
    let outs_in_conf = res
        .outputs
        .iter()
        .filter_map(|o| setup.get(&o).map(|c| (c, o)));
    // This loop can't easily be a map, as it needs to be able to use '?'
    for (&conf, &out) in outs_in_conf {
        let out_info = conn
            .randr_get_output_info(out, timestamp)
            .into_diagnostic()?
            .reply()
            .into_diagnostic()?;
        let mode = find_mode_id(&out_info, &modes, &conf.mode)?;
        let dest_crtc = allocate_crtc(&out_info, &mut free_crtcs)
            .ok_or_else(|| Error::NoCrtc(conf.name.clone()))
            .into_diagnostic()?;
        //TODO: This is not a correct computation of the screen size
        mm_w += out_info.mm_width;
        mm_h += out_info.mm_height;
        let Position { x, y } = conf.position;
        let crtc_info = conn
            .randr_get_crtc_info(dest_crtc, timestamp)
            .into_diagnostic()?
            .reply()
            .into_diagnostic()?;
        if x != crtc_info.x || y != crtc_info.y || mode != crtc_info.mode {
            enables.push(SetCrtcConfigRequest {
                x,
                y,
                rotation: 1,
                mode,
                outputs: vec![out].into(),
                ..disable_crtc(dest_crtc, &crtc_info)
            });
        }
    }
    // If there were CRTCs left over after allocating the next setup, ensure that they are
    // disabled
    let mut disables = Vec::with_capacity(free_crtcs.len());
    for &crtc in free_crtcs.into_iter() {
        let info = conn
            .randr_get_crtc_info(crtc, timestamp)
            .into_diagnostic()?
            .reply()
            .into_diagnostic()?;
        if !info.outputs.is_empty() || info.mode != 0 {
            disables.push(disable_crtc(crtc, &info));
        }
    }

    let geom = conn
        .get_geometry(root)
        .into_diagnostic()?
        .reply()
        .into_diagnostic()?;
    let mut current = Mode {
        w: geom.width,
        h: geom.height,
    };
    if disables.is_empty() && enables.is_empty() && &current == fb_size {
        Ok(false)
    } else {
        // First, we disable any CTRCs that must be disabled
        if !disables.is_empty() {
            info!("Disabling CRTCs {:?}", disables);
            batch_config(conn, disables)?;
        }
        // Then we change the screen size to be large enough for both configuration
        if current != current.union(fb_size) {
            current = current.union(fb_size);
            info!(
                "Before Config - Setting Screen {} Size to {}x{} {}mmx{}mm",
                root, current.w, current.h, mm_w, mm_h
            );
            conn.randr_set_screen_size(root, current.w, current.h, mm_w, mm_h)
                .into_diagnostic()?
                .check()
                .into_diagnostic()?;
        }
        // Finally we enable and change modes of CRTCs
        batch_config(conn, enables)?;
        // Lastly we change the screen size to be the correct size for the final config
        if &current != fb_size {
            conn.randr_set_screen_size(root, fb_size.w, fb_size.h, mm_w, mm_h)
                .into_diagnostic()?
                .check()
                .into_diagnostic()?;
            info!(
                "After Config - Setting Screen Size to {}x{}",
                fb_size.w, fb_size.h
            );
        }
        Ok(true)
    }
}

/// Called for each screen change notificaiton. Detects connected monitors and switches
/// to the appropriate config.
fn switch_setup<C: Connection>(
    config: &Config,
    conn: &C,
    edid: Atom,
    root: Window,
    force_print: bool,
) -> () {
    let res = match get_outputs(conn, root) {
        Ok(o) => o,
        Err(e) => {
            error!("{:?}", e);
            return;
        }
    };
    match get_config(&config, conn, &res.outputs, edid) {
        Some((name, fb_size, setup)) => match apply_config(conn, &res, fb_size, setup, root) {
            Ok(changed) => {
                if changed || force_print {
                    println!("Monitor configuration: {}", name)
                }
            }
            Err(e) => error!("{:?}", e),
        },
        None => error!(
            "Error: Monitor change indicated, and the connected monitors did not match a config"
        ),
    }
}

fn setup_notify<C: Connection>(conn: &C, root: Window, mask: NotifyMask) -> Result<()> {
    conn.randr_select_input(root, mask)
        .into_diagnostic()?
        .check()
        .into_diagnostic()?;
    Ok(())
}

pub fn daemon(args: &ArgMatches<'_>) -> Result<()> {
    let config = check(args)?;
    if !args.is_present("check") {
        let (conn, screen_num) = ok_or_exit(connect(None), |e| {
            eprintln!("Could not connect to X server: {}", e);
            1
        });
        let setup = conn.setup();
        let atom_edid = ok_or_exit(edid_atom(&conn), |e| {
            eprintln!("Failed to intern EDID atom: {}", e);
            1
        });
        let root = setup.roots[screen_num].root;
        let notify_mask =
            NotifyMask::SCREEN_CHANGE | NotifyMask::OUTPUT_CHANGE | NotifyMask::CRTC_CHANGE;
        ok_or_exit(setup_notify(&conn, root, notify_mask), |e| {
            eprintln!("Could not enable notifications: {}", e);
            1
        });
        switch_setup(&config, &conn, atom_edid, root, true);
        loop {
            match conn.wait_for_event() {
                Ok(Event::RandrScreenChangeNotify(_)) => {
                    switch_setup(&config, &conn, atom_edid, root, false)
                }
                _ => (),
            }
        }
    }
    Ok(())
}

pub fn check(args: &ArgMatches<'_>) -> Result<Config> {
    // Unwrap below is safe, because the program exits from `get_matches` above when a config
    // is not provided.
    let config_name = args.value_of("config").unwrap();
    Config::from_fname(&config_name).into_diagnostic()
}
