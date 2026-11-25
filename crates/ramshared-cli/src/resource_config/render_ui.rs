// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/render_ui.rs
//! Text/JSON config rendering and interactive TUI resource editor screen.

use super::{
    BYTES_PER_MIB, InventoryError, ResourceProfile, ResourceSnapshot, RuntimePlatform,
    collect_snapshot, draft_wsl_target_path_is_valid, editor_apply, editor_save, gpu_inventory,
    load_profile_text, recommended, tier_editor, unix_millis, windows_volume_eligibility,
};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::Duration;

pub(super) fn format_gib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "unavailable".into(),
        |bytes| format!("{:.2} GiB", bytes as f64 / (1024_f64 * 1024_f64 * 1024_f64)),
    )
}

pub(super) fn format_cap_mib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "not set".into(),
        |bytes| {
            if bytes % BYTES_PER_MIB == 0 {
                format!("{} MiB", bytes / BYTES_PER_MIB)
            } else {
                format!("{bytes} bytes")
            }
        },
    )
}

pub(super) fn render_text(snapshot: &ResourceSnapshot) -> String {
    render_text_with_preset(snapshot, Some(gpu_inventory::Preset::Recommended))
}

pub(super) fn render_text_with_preset(
    snapshot: &ResourceSnapshot,
    preset: Option<gpu_inventory::Preset>,
) -> String {
    let mut output = String::new();
    let platform = match snapshot.platform {
        RuntimePlatform::NativeLinux => "Native Linux",
        RuntimePlatform::Wsl2 => "WSL2",
    };
    let _ = writeln!(output, "RamShared resource inventory — {platform}");
    match snapshot.platform {
        RuntimePlatform::NativeLinux => {
            let _ = writeln!(
                output,
                "System RAM: total {}, available {}",
                format_gib(snapshot.guest_memory.total_bytes),
                format_gib(snapshot.guest_memory.available_bytes)
            );
        }
        RuntimePlatform::Wsl2 => {
            let _ = writeln!(
                output,
                "WSL guest RAM: total {}, available {}",
                format_gib(snapshot.guest_memory.total_bytes),
                format_gib(snapshot.guest_memory.available_bytes)
            );
            if let Some(windows) = &snapshot.windows {
                let _ = writeln!(
                    output,
                    "Windows host RAM: total {}, physically free {}",
                    format_gib(windows.host_memory.total_bytes),
                    format_gib(windows.host_memory.free_bytes)
                );
                let _ = writeln!(
                    output,
                    "Windows commit headroom: {}",
                    format_gib(windows.host_memory.commit_headroom_bytes())
                );
            } else {
                let _ = writeln!(output, "Windows host RAM: unavailable");
                let _ = writeln!(output, "Windows commit headroom: unavailable");
            }
        }
    }

    let _ = writeln!(
        output,
        "Guest swap: total {}, free {}",
        format_gib(snapshot.guest_memory.swap_total_bytes),
        format_gib(snapshot.guest_memory.swap_free_bytes)
    );
    for swap in &snapshot.swaps {
        let _ = writeln!(
            output,
            "  {} ({}, priority {}): {} KiB / {} KiB used",
            swap.filename, swap.kind, swap.priority, swap.used_kib, swap.size_kib
        );
    }
    if let Some(preset) = preset {
        output.push_str(&gpu_inventory::render_at(
            &snapshot.gpu_inventory,
            preset,
            snapshot.gpu_observed_unix_ms,
            unix_millis(),
        ));
    }

    let _ = writeln!(output, "Linux block devices and filesystems:");
    if snapshot.block_devices.is_empty() {
        let _ = writeln!(output, "  unavailable");
    }
    for device in &snapshot.block_devices {
        let mount = if device.mountpoints.is_empty() {
            "unmounted".into()
        } else {
            device.mountpoints.join(", ")
        };
        let hardware_identity = device
            .parent_hardware_identity
            .as_deref()
            .or(device.hardware_identity.as_deref())
            .unwrap_or("unknown");
        let rotation = match device.rotational {
            Some(true) => "rotating",
            Some(false) => "non-rotating",
            None => "rotation unknown",
        };
        let _ = writeln!(
            output,
            "  {} {} ({}, {}, mounts {}, model {}, transport {}, {}, ID {}) — {}",
            device.name,
            format_gib(device.size_bytes),
            device.kind,
            device.filesystem.as_deref().unwrap_or("filesystem unknown"),
            mount,
            device.model.as_deref().unwrap_or("unknown"),
            device.transport.as_deref().unwrap_or("unknown"),
            rotation,
            hardware_identity,
            device.eligibility_reason
        );
        for mount in &device.mounts {
            let (free_bytes, total_bytes, backing) = match mount.host_backing.as_ref() {
                Some(host) => (
                    Some(host.free_bytes),
                    Some(host.total_bytes),
                    format!("; host-backed by {}: VHDX volume", host.drive_letter),
                ),
                None => (mount.available_bytes, mount.total_bytes, String::new()),
            };
            let _ = writeln!(
                output,
                "    {} [{} mount {}, device {}] — free {} of {}{}{}",
                mount.mountpoint,
                mount.filesystem,
                mount.mount_id,
                mount.major_minor,
                format_gib(free_bytes),
                format_gib(total_bytes),
                if mount.read_only { "; read-only" } else { "" },
                backing
            );
        }
    }

    if snapshot.platform == RuntimePlatform::Wsl2 {
        let _ = writeln!(output, "Windows volumes:");
        match &snapshot.windows {
            Some(windows) if windows.volumes.is_empty() => {
                let _ = writeln!(output, "  none reported");
            }
            Some(windows) => {
                for volume in &windows.volumes {
                    let drive = volume
                        .drive_letter
                        .as_deref()
                        .map_or_else(|| "(no drive letter)".into(), |letter| format!("{letter}:"));
                    let identity = volume
                        .volume_id
                        .as_deref()
                        .filter(|identity| !identity.trim().is_empty());
                    let matching_id_count = identity.map_or(0, |identity| {
                        windows
                            .volumes
                            .iter()
                            .filter(|candidate| {
                                candidate
                                    .volume_id
                                    .as_deref()
                                    .is_some_and(|observed| observed.eq_ignore_ascii_case(identity))
                            })
                            .count()
                    });
                    let eligibility = match windows_volume_eligibility(volume, matching_id_count) {
                        Ok(_)
                            if identity.is_some_and(|identity| {
                                draft_wsl_target_path_is_valid(
                                    identity,
                                    volume.drive_letter.as_deref(),
                                )
                            }) =>
                        {
                            "eligible volume candidate".to_owned()
                        }
                        Ok(_) => "ineligible: no safe canonical target path".to_owned(),
                        Err((_, reason)) => format!("ineligible: {reason}"),
                    };
                    let identity = identity.map_or_else(
                        || "identity unknown".to_owned(),
                        |identity| format!("ID {identity}"),
                    );
                    let _ = writeln!(
                        output,
                        "  {drive} {} {} ({}) — {identity} — {eligibility} — free {} of {}",
                        volume.label.as_deref().unwrap_or("unlabeled"),
                        volume
                            .file_system
                            .as_deref()
                            .unwrap_or("filesystem unknown"),
                        volume.drive_type,
                        format_gib(volume.free_bytes),
                        format_gib(volume.size_bytes)
                    );
                }
            }
            None => {
                let _ = writeln!(output, "  unavailable");
            }
        }
    }
    if let Some(diag) = super::benchmark::windows_host::read_latest_diagnostic_for(snapshot) {
        for line in super::benchmark::windows_host::format_diagnostic_lines(&diag) {
            let _ = writeln!(output, "{line}");
        }
    } else {
        let _ = writeln!(
            output,
            "Storage speed comparison: not measured by this read-only inventory."
        );
    }
    let _ = writeln!(output, "GPU/VRAM budget: {}", snapshot.gpu_budget_status);
    if let Some(preset) = preset {
        let rec = recommended::recommend_three_tiers(snapshot, preset, unix_millis());
        output.push_str(&recommended::render_recommendation_summary(&rec));
    }
    for warning in &snapshot.warnings {
        let _ = writeln!(output, "Warning: {warning}");
    }
    let _ = writeln!(
        output,
        "Read-only inventory. No swap, origin, or GPU allocation was changed."
    );
    output
}

pub(super) fn render_json(snapshot: &ResourceSnapshot) -> Result<String, InventoryError> {
    let mut value = serde_json::to_value(snapshot)
        .map_err(|error| InventoryError(format!("cannot serialize inventory: {error}")))?;
    if let serde_json::Value::Object(map) = &mut value {
        if let Some(diag) = super::benchmark::windows_host::read_latest_diagnostic_for(snapshot)
            && let Ok(diag_val) = serde_json::to_value(&diag)
        {
            map.insert("latest_benchmark".into(), diag_val);
        }
        let rec = recommended::recommend_three_tiers(
            snapshot,
            gpu_inventory::Preset::Recommended,
            unix_millis(),
        );
        if let Ok(rec_val) = serde_json::to_value(&rec) {
            map.insert("recommended_three_tiers".into(), rec_val);
        }
        let obs = super::assistant::observe_live(snapshot);
        let checklist = super::assistant::build_checklist(&obs);
        if let Ok(caps_val) = serde_json::to_value(&checklist.capabilities) {
            map.insert("kernel_capabilities".into(), caps_val);
        }
        if let Ok(check_val) = serde_json::to_value(&checklist) {
            map.insert("assistant_checklist".into(), check_val);
        }
    }
    serde_json::to_string(&value)
        .map_err(|error| InventoryError(format!("cannot serialize inventory: {error}")))
}

pub(super) fn draw_config_frame(
    frame: &mut Frame<'_>,
    snapshot: &ResourceSnapshot,
    scroll: u16,
    editor: &tier_editor::Editor,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let title = Paragraph::new("RamShared Resource Configuration — draft editor")
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);
    let body = Paragraph::new(format!(
        "{}\n{}",
        editor.render(unix_millis()),
        render_text_with_preset(snapshot, None)
    ))
    .wrap(Wrap { trim: false })
    .scroll((scroll, 0))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(body, chunks[1]);
    let footer = Line::styled(
        "Tab tier  ←/→ size  v volume  p preset  a apply  s save draft  w wizard  Home/End limits  ↑/↓ scroll  r refresh  q close",
        Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
    );
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), chunks[2]);
}

pub(super) enum ActiveModal {
    Save(editor_save::SaveDialog),
    Apply(editor_apply::ApplyDialog),
}

pub(super) fn run_interactive() -> Result<(), InventoryError> {
    run_interactive_profile(None)
}

pub(super) fn prepare_interactive_session(
    path: Option<&str>,
    now_unix_ms: u64,
    collect_fn: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
) -> Result<(ResourceSnapshot, tier_editor::Editor), InventoryError> {
    let profile = path
        .map(|path| {
            let text = load_profile_text(Path::new(path), false)?
                .ok_or_else(|| InventoryError("requested profile file does not exist".into()))?;
            ResourceProfile::parse(&text).map_err(|error| InventoryError(error.to_string()))
        })
        .transpose()?;
    let snapshot = collect_fn()?;
    let editor = if let Some(profile) = &profile {
        tier_editor::Editor::from_profile(&snapshot, profile, now_unix_ms)?
    } else {
        tier_editor::Editor::from_snapshot_recommended(&snapshot, now_unix_ms)
    };
    Ok((snapshot, editor))
}

pub(super) fn draw_interactive_state(
    frame: &mut Frame<'_>,
    snapshot: &ResourceSnapshot,
    scroll: u16,
    editor: &tier_editor::Editor,
    modal: Option<&ActiveModal>,
) {
    match modal {
        Some(ActiveModal::Save(dialog)) => dialog.draw(frame),
        Some(ActiveModal::Apply(dialog)) => dialog.draw(frame),
        None => draw_config_frame(frame, snapshot, scroll, editor),
    }
}

pub(super) fn run_interactive_profile(path: Option<&str>) -> Result<(), InventoryError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(InventoryError(
            "interactive config needs a terminal; use `ramshared config show`".into(),
        ));
    }
    let (mut snapshot, mut editor) =
        prepare_interactive_session(path, unix_millis(), &mut collect_snapshot)?;
    let mut terminal = ratatui::init();
    let result = (|| -> Result<EditorLoopControl, InventoryError> {
        let mut scroll = 0u16;
        let mut modal: Option<ActiveModal> = None;
        loop {
            terminal
                .draw(|frame| {
                    draw_interactive_state(frame, &snapshot, scroll, &editor, modal.as_ref());
                })
                .map_err(|error| InventoryError(format!("cannot draw config screen: {error}")))?;
            if !ratatui::crossterm::event::poll(Duration::from_millis(250))
                .map_err(|error| InventoryError(format!("cannot read config input: {error}")))?
            {
                continue;
            }
            if let ratatui::crossterm::event::Event::Key(key) = ratatui::crossterm::event::read()
                .map_err(|error| InventoryError(format!("cannot read config input: {error}")))?
            {
                if key.kind == ratatui::crossterm::event::KeyEventKind::Release {
                    continue;
                }
                match handle_interactive_key(
                    key.code,
                    &mut snapshot,
                    &mut scroll,
                    &mut editor,
                    &mut modal,
                    unix_millis(),
                    &mut collect_snapshot,
                )? {
                    EditorLoopControl::Continue => {}
                    EditorLoopControl::Break => return Ok(EditorLoopControl::Break),
                    EditorLoopControl::OpenWizard => return Ok(EditorLoopControl::OpenWizard),
                }
            }
        }
    })();
    ratatui::restore();
    match result? {
        EditorLoopControl::OpenWizard => super::assistant_ui::run_interactive_assistant(),
        EditorLoopControl::Break | EditorLoopControl::Continue => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorLoopControl {
    Continue,
    Break,
    OpenWizard,
}

pub(super) fn handle_interactive_key(
    code: KeyCode,
    snapshot: &mut ResourceSnapshot,
    scroll: &mut u16,
    editor: &mut tier_editor::Editor,
    modal: &mut Option<ActiveModal>,
    now_unix_ms: u64,
    collect_fn: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
) -> Result<EditorLoopControl, InventoryError> {
    if let Some(active) = modal {
        if code == KeyCode::Esc {
            *modal = None;
        } else {
            let mut observe = || {
                let fresh = collect_fn()?;
                *snapshot = fresh.clone();
                Ok(fresh)
            };
            match active {
                ActiveModal::Save(dialog) => dialog.input(code, editor, &mut observe),
                ActiveModal::Apply(dialog) => dialog.input(code, editor, &mut observe),
            }
        }
        return Ok(EditorLoopControl::Continue);
    }
    match code {
        KeyCode::Char('s') => *modal = Some(ActiveModal::Save(editor_save::SaveDialog::default())),
        KeyCode::Char('a') => {
            *modal = Some(ActiveModal::Apply(
                editor_apply::ApplyDialog::new_production(editor, now_unix_ms),
            ));
        }
        KeyCode::Char('w') => return Ok(EditorLoopControl::OpenWizard),
        KeyCode::Char('q') | KeyCode::Esc => return Ok(EditorLoopControl::Break),
        KeyCode::Down => *scroll = scroll.saturating_add(1),
        KeyCode::Up => *scroll = scroll.saturating_sub(1),
        KeyCode::Char('r') => {
            *snapshot = collect_fn()?;
            editor.reconcile(tier_editor::Editor::from_snapshot(snapshot, now_unix_ms));
        }
        KeyCode::Home => editor.clear_selected(),
        KeyCode::End => editor.fill_selected(now_unix_ms),
        KeyCode::Tab => editor.next(),
        KeyCode::Left => editor.adjust_selected(false, now_unix_ms),
        KeyCode::Right => editor.adjust_selected(true, now_unix_ms),
        KeyCode::Char('v') => editor.cycle_selected(now_unix_ms),
        KeyCode::Char('p') => editor.cycle_all_tier_preset(now_unix_ms),
        _ => {}
    }
    Ok(EditorLoopControl::Continue)
}
