// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/assistant_ui.rs
//! Interactive Ratatui TUI and headless CLI runner for the unified RamShared
//! Setup & Configuration Assistant (`ramshared config wizard` / `setup wizard`).
//!

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::assistant::{
    AUTO_UPDATE_CONF_FILE, AssistantChecklist, ItemStatus, build_checklist,
    kernel_upgrade_guidance_text, observe_live, parse_auto_update_conf, render_auto_update_conf,
    render_checklist_json, render_checklist_text, write_auto_update_conf_atomic,
};
use super::gpu_inventory::Preset;
use super::recommended::{recommend_three_tiers, render_cascade_conf, write_cascade_conf_atomic};
use super::{InventoryError, ResourceSnapshot, collect_snapshot, render_ui, unix_millis};
use crate::cascade::CASCADE_CONF_FILE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssistantControl {
    Continue,
    Quit,
    OpenTierEditor,
}

pub(super) enum AssistantModal {
    ApplyCascade {
        preview: String,
        confirmation: String,
        message: String,
        done: bool,
    },
    ToggleAutoUpdate {
        target_enabled: bool,
        preview: String,
        confirmation: String,
        message: String,
        done: bool,
    },
    KernelGuidance {
        body: String,
    },
}

pub(super) struct AssistantScreen {
    snapshot: ResourceSnapshot,
    checklist: AssistantChecklist,
    selected: usize,
    modal: Option<AssistantModal>,
    banner: String,
    cascade_conf_path: PathBuf,
    auto_update_conf_path: PathBuf,
}

impl AssistantScreen {
    pub(super) fn new(
        snapshot: ResourceSnapshot,
        checklist: AssistantChecklist,
        cascade_conf_path: PathBuf,
        auto_update_conf_path: PathBuf,
    ) -> Self {
        Self {
            snapshot,
            checklist,
            selected: 0,
            modal: None,
            banner: String::new(),
            cascade_conf_path,
            auto_update_conf_path,
        }
    }

    pub(super) fn from_live() -> Result<Self, InventoryError> {
        let snapshot = collect_snapshot()?;
        let obs = observe_live(&snapshot);
        let checklist = build_checklist(&obs);
        Ok(Self::new(
            snapshot,
            checklist,
            PathBuf::from(CASCADE_CONF_FILE),
            PathBuf::from(AUTO_UPDATE_CONF_FILE),
        ))
    }

    pub(super) fn open_apply_cascade_modal(&mut self, now_unix_ms: u64) {
        let rec = recommend_three_tiers(&self.snapshot, Preset::Recommended, now_unix_ms);
        match render_cascade_conf(
            rec.cascade_zram_mib,
            rec.cascade_vram_mib,
            rec.min_vram_headroom_mib,
            "ramshared config wizard (hardware recommended)",
        ) {
            Ok(preview) => {
                self.modal = Some(AssistantModal::ApplyCascade {
                    preview,
                    confirmation: String::new(),
                    message: String::new(),
                    done: false,
                });
            }
            Err(err) => self.banner = format!("Cannot prepare cascade.conf: {err}"),
        }
    }

    pub(super) fn open_toggle_auto_update_modal(&mut self) {
        let existing = std::fs::read_to_string(&self.auto_update_conf_path)
            .map(|text| parse_auto_update_conf(&text))
            .unwrap_or_default();
        let target_enabled = existing.enabled != Some(true);
        let preview = render_auto_update_conf(&existing, target_enabled);
        self.modal = Some(AssistantModal::ToggleAutoUpdate {
            target_enabled,
            preview,
            confirmation: String::new(),
            message: String::new(),
            done: false,
        });
    }

    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        now_unix_ms: u64,
        refresh: &mut dyn FnMut() -> Result<(ResourceSnapshot, AssistantChecklist), InventoryError>,
    ) -> AssistantControl {
        if self.modal.is_some() {
            self.handle_modal_key(code, refresh);
            return AssistantControl::Continue;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return AssistantControl::Quit,
            KeyCode::Char('e') => return AssistantControl::OpenTierEditor,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.checklist.items.is_empty() {
                    self.selected = (self.selected + 1) % self.checklist.items.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.checklist.items.is_empty() {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(self.checklist.items.len() - 1);
                }
            }
            KeyCode::Char('a') => self.open_apply_cascade_modal(now_unix_ms),
            KeyCode::Char('u') => self.open_toggle_auto_update_modal(),
            KeyCode::Char('g') | KeyCode::Enter => {
                self.modal = Some(AssistantModal::KernelGuidance {
                    body: kernel_upgrade_guidance_text(&self.checklist.capabilities),
                });
            }
            KeyCode::Char('r') => match refresh() {
                Ok((snapshot, checklist)) => {
                    self.snapshot = snapshot;
                    self.checklist = checklist;
                    self.banner = "Refreshed live hardware and kernel capabilities.".into();
                }
                Err(err) => self.banner = format!("Refresh failed: {err}"),
            },
            _ => {}
        }
        AssistantControl::Continue
    }

    fn handle_modal_key(
        &mut self,
        code: KeyCode,
        refresh: &mut dyn FnMut() -> Result<(ResourceSnapshot, AssistantChecklist), InventoryError>,
    ) {
        if code == KeyCode::Esc {
            self.modal = None;
            return;
        }
        let Some(modal) = &mut self.modal else {
            return;
        };
        match modal {
            AssistantModal::KernelGuidance { .. } => {
                if matches!(code, KeyCode::Char('q') | KeyCode::Enter) {
                    self.modal = None;
                }
            }
            AssistantModal::ApplyCascade {
                preview,
                confirmation,
                message,
                done,
            } => {
                if *done {
                    return;
                }
                match code {
                    KeyCode::Backspace => {
                        confirmation.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() && confirmation.len() < 5 => {
                        confirmation.push(c);
                    }
                    KeyCode::Enter => {
                        if confirmation != "APPLY" {
                            *message = "type APPLY exactly, or Esc to cancel".into();
                            return;
                        }
                        match write_cascade_conf_atomic(&self.cascade_conf_path, preview, true) {
                            Ok(_) => {
                                *done = true;
                                *message = format!(
                                    "Saved {}. Live swap tiers were not disrupted. Esc returns.",
                                    self.cascade_conf_path.display()
                                );
                                if let Ok((snap, list)) = refresh() {
                                    self.snapshot = snap;
                                    self.checklist = list;
                                }
                            }
                            Err(err) => {
                                *message = format!(
                                    "{err} (if permission denied, run `sudo ramshared config wizard`)"
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            AssistantModal::ToggleAutoUpdate {
                target_enabled,
                confirmation,
                message,
                done,
                ..
            } => {
                if *done {
                    return;
                }
                match code {
                    KeyCode::Backspace => {
                        confirmation.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() && confirmation.len() < 5 => {
                        confirmation.push(c);
                    }
                    KeyCode::Enter => {
                        if confirmation != "APPLY" {
                            *message = "type APPLY exactly, or Esc to cancel".into();
                            return;
                        }
                        match write_auto_update_conf_atomic(
                            &self.auto_update_conf_path,
                            *target_enabled,
                        ) {
                            Ok(_) => {
                                *done = true;
                                *message = format!(
                                    "Updated {} (enabled={}). Esc returns.",
                                    self.auto_update_conf_path.display(),
                                    if *target_enabled { 1 } else { 0 }
                                );
                                if let Ok((snap, list)) = refresh() {
                                    self.snapshot = snap;
                                    self.checklist = list;
                                }
                            }
                            Err(err) => {
                                *message = format!(
                                    "{err} (if permission denied, run `sudo ramshared config wizard`)"
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

pub(super) fn draw_assistant_frame(frame: &mut Frame<'_>, screen: &AssistantScreen) {
    if let Some(modal) = &screen.modal {
        draw_modal(frame, screen, modal);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(6),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let title = format!(
        "RamShared — Unified Setup & Configuration Assistant ({} OK · {} pending/optional)",
        screen.checklist.ok_count, screen.checklist.pending_count
    );
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        chunks[0],
    );

    let mut lines = Vec::new();
    for (idx, item) in screen.checklist.items.iter().enumerate() {
        let marker = if idx == screen.selected { ">" } else { " " };
        let color = match item.status {
            ItemStatus::Ok => Color::Green,
            ItemStatus::Pending | ItemStatus::Attention => Color::Yellow,
            ItemStatus::Optional | ItemStatus::Unavailable => Color::Cyan,
        };
        let style = if idx == screen.selected {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        lines.push(Line::styled(
            format!(
                "{marker} {:<11} {}. {} — {}",
                item.status.badge(),
                idx + 1,
                item.title,
                item.summary
            ),
            style,
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Setup Checklist"),
        ),
        chunks[1],
    );

    let detail_text =
        screen
            .checklist
            .items
            .get(screen.selected)
            .map_or_else(String::new, |item| {
                let mut text = format!("{}\nNext: {}", item.detail, item.action_hint);
                if !screen.banner.is_empty() {
                    text.push('\n');
                    text.push_str(&screen.banner);
                }
                text
            });
    frame.render_widget(
        Paragraph::new(detail_text)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Selected Item"),
            ),
        chunks[2],
    );

    let footer = Line::styled(
        "↑/↓ select  a apply 3-tier conf  u toggle auto-update  e tier bar editor  g kernel guide  r refresh  q close",
        Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
    );
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), chunks[3]);
}

fn draw_modal(frame: &mut Frame<'_>, screen: &AssistantScreen, modal: &AssistantModal) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    match modal {
        AssistantModal::KernelGuidance { body } => {
            frame.render_widget(
                Paragraph::new(body.as_str())
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Kernel Optimization Guide (Esc / Enter to close)"),
                    ),
                area,
            );
        }
        AssistantModal::ApplyCascade {
            preview,
            confirmation,
            message,
            done,
        } => {
            draw_confirm_modal(
                frame,
                "Apply recommended 3-tier cascade.conf",
                &format!(
                    "Target: {}\n\n{preview}",
                    screen.cascade_conf_path.display()
                ),
                confirmation,
                message,
                *done,
            );
        }
        AssistantModal::ToggleAutoUpdate {
            target_enabled,
            preview,
            confirmation,
            message,
            done,
        } => {
            let verb = if *target_enabled { "Enable" } else { "Disable" };
            draw_confirm_modal(
                frame,
                &format!("{verb} signed auto-update"),
                &format!(
                    "Target: {}\n\n{preview}",
                    screen.auto_update_conf_path.display()
                ),
                confirmation,
                message,
                *done,
            );
        }
    }
}

fn draw_confirm_modal(
    frame: &mut Frame<'_>,
    title: &str,
    body: &str,
    confirmation: &str,
    message: &str,
    done: bool,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(4)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(title)),
        rows[0],
    );
    let prompt = if done {
        String::new()
    } else {
        format!("Type APPLY: {confirmation}  (Esc cancel)")
    };
    frame.render_widget(
        Paragraph::new(format!("{prompt}\n{message}"))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Confirmation")),
        rows[1],
    );
}

pub(crate) fn run_wizard(json: bool, stdout: &mut dyn Write) -> Result<(), InventoryError> {
    run_wizard_with_tty(
        json,
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        stdout,
    )
}

fn run_wizard_with_tty(
    json: bool,
    interactive_tty: bool,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    let snapshot = collect_snapshot()?;
    let obs = observe_live(&snapshot);
    let checklist = build_checklist(&obs);
    if json {
        let out = render_checklist_json(&checklist)?;
        return writeln!(stdout, "{out}")
            .map_err(|err| InventoryError(format!("cannot write wizard JSON: {err}")));
    }
    if !interactive_tty {
        return writeln!(stdout, "{}", render_checklist_text(&checklist))
            .map_err(|err| InventoryError(format!("cannot write wizard output: {err}")));
    }
    run_interactive_assistant()
}

pub(super) fn run_interactive_assistant() -> Result<(), InventoryError> {
    let mut screen = AssistantScreen::from_live()?;
    let mut terminal = ratatui::init();
    let outcome = (|| -> Result<AssistantControl, InventoryError> {
        loop {
            terminal
                .draw(|frame| draw_assistant_frame(frame, &screen))
                .map_err(|err| InventoryError(format!("cannot draw assistant screen: {err}")))?;
            if !ratatui::crossterm::event::poll(Duration::from_millis(250))
                .map_err(|err| InventoryError(format!("cannot poll assistant input: {err}")))?
            {
                continue;
            }
            if let ratatui::crossterm::event::Event::Key(key) = ratatui::crossterm::event::read()
                .map_err(|err| InventoryError(format!("cannot read assistant input: {err}")))?
            {
                if key.kind == ratatui::crossterm::event::KeyEventKind::Release {
                    continue;
                }
                let mut refresh = || {
                    let snap = collect_snapshot()?;
                    let obs = observe_live(&snap);
                    Ok((snap, build_checklist(&obs)))
                };
                match screen.handle_key(key.code, unix_millis(), &mut refresh) {
                    AssistantControl::Continue => {}
                    AssistantControl::Quit => return Ok(AssistantControl::Quit),
                    AssistantControl::OpenTierEditor => {
                        return Ok(AssistantControl::OpenTierEditor);
                    }
                }
            }
        }
    })();
    ratatui::restore();
    match outcome? {
        AssistantControl::OpenTierEditor => render_ui::run_interactive(),
        AssistantControl::Quit | AssistantControl::Continue => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::resource_config::assistant::{AssistantObservation, AutoUpdateFileState};
    use crate::resource_config::kernel_capabilities::{
        AdaptiveModePlan, CapabilityState, KernelCapabilities, MglruState, UblkState,
    };
    use crate::resource_config::{MemorySnapshot, RuntimePlatform, prepared_origin};

    fn fixture_snapshot() -> ResourceSnapshot {
        ResourceSnapshot {
            platform: RuntimePlatform::Wsl2,
            observed_unix_ms: 1_000,
            guest_observed_unix_ms: 1_000,
            gpu_observed_unix_ms: 1_000,
            guest_memory: MemorySnapshot {
                total_bytes: Some(16 * 1024 * 1024 * 1024),
                available_bytes: Some(8 * 1024 * 1024 * 1024),
                swap_total_bytes: Some(4 * 1024 * 1024 * 1024),
                swap_free_bytes: Some(2 * 1024 * 1024 * 1024),
                required_counters_available: true,
            },
            swaps: Vec::new(),
            block_devices: Vec::new(),
            windows: None,
            gpu_inventory: Vec::new(),
            gpu_budget_status: "not sampled".into(),
            warnings: Vec::new(),
            prepared_native_origin: prepared_origin::Observation::Absent,
        }
    }

    fn fixture_checklist() -> AssistantChecklist {
        let caps = KernelCapabilities {
            platform: RuntimePlatform::Wsl2,
            kernel_release: Some("6.18.64.1-microsoft-standard-WSL2+".into()),
            zram_writeback: CapabilityState::Supported,
            zram_actime: CapabilityState::Unsupported,
            zram_algorithms: vec!["lzo-rle".into()],
            zram_active_algorithm: Some("lzo-rle".into()),
            zram_multi_comp: CapabilityState::Unsupported,
            mglru: MglruState::Absent,
            psi: CapabilityState::Supported,
            ublk: UblkState::PresentLabGatedWsl2,
            nbd_present: true,
            vmbus_v3_active: CapabilityState::Supported,
            effective_transport: "nbd",
            kernel_gaps: Vec::new(),
            adaptive_plan: AdaptiveModePlan {
                primary_zram_algo: "lzo-rle".into(),
                secondary_recomp_algo: None,
                spill_aging_mode: "two_phase_adaptive",
                tier2_transport: "nbd",
                transport_note: "NBD active",
            },
        };
        build_checklist(&AssistantObservation {
            installed_release: Some("v0.14.1-1448-gabcdef01".into()),
            cascade_conf_present: false,
            cascade_conf_summary: None,
            gpu_count: 0,
            gpu_cache_active: None,
            control_plane_state: None,
            cascade_service_enabled: Some(true),
            auto_update_timer_enabled: Some(true),
            auto_update_conf: AutoUpdateFileState::default(),
            capabilities: caps,
        })
    }

    fn render_buffer(screen: &AssistantScreen) -> String {
        let backend = ratatui::backend::TestBackend::new(120, 32);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_assistant_frame(frame, screen))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn assistant_tui_renders_checklist_handles_navigation_and_apply_refusal() {
        let dir = std::env::temp_dir().join(format!("ramshared-aui-1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut screen = AssistantScreen::new(
            fixture_snapshot(),
            fixture_checklist(),
            dir.join("cascade.conf"),
            dir.join("auto-update.conf"),
        );
        let initial = render_buffer(&screen);
        assert!(initial.contains("Unified Setup & Configuration Assistant"));
        assert!(initial.contains("Setup Checklist"));

        let mut refresh = || Ok((fixture_snapshot(), fixture_checklist()));
        assert_eq!(
            screen.handle_key(KeyCode::Down, 1_000, &mut refresh),
            AssistantControl::Continue
        );
        assert_eq!(screen.selected, 1);
        assert_eq!(
            screen.handle_key(KeyCode::Up, 1_000, &mut refresh),
            AssistantControl::Continue
        );
        assert_eq!(screen.selected, 0);
        // Open kernel guide modal and close with Enter
        screen.handle_key(KeyCode::Char('g'), 1_000, &mut refresh);
        assert!(render_buffer(&screen).contains("Kernel Optimization Guide"));
        screen.handle_key(KeyCode::Enter, 1_000, &mut refresh);
        assert!(screen.modal.is_none());

        // Open cascade.conf modal, submit wrong confirmation -> refused
        screen.handle_key(KeyCode::Char('a'), 1_000, &mut refresh);
        for c in "NO".chars() {
            screen.handle_key(KeyCode::Char(c), 1_000, &mut refresh);
        }
        screen.handle_key(KeyCode::Enter, 1_000, &mut refresh);
        assert!(!dir.join("cascade.conf").exists());
        assert!(render_buffer(&screen).contains("type APPLY exactly"));
        // Clear with Backspace, type APPLY, commit atomically
        screen.handle_key(KeyCode::Backspace, 1_000, &mut refresh);
        screen.handle_key(KeyCode::Backspace, 1_000, &mut refresh);
        for c in "APPLY".chars() {
            screen.handle_key(KeyCode::Char(c), 1_000, &mut refresh);
        }
        screen.handle_key(KeyCode::Enter, 1_000, &mut refresh);
        assert!(dir.join("cascade.conf").exists());
        screen.handle_key(KeyCode::Esc, 1_000, &mut refresh);
        assert_eq!(
            screen.handle_key(KeyCode::Char('r'), 1_000, &mut refresh),
            AssistantControl::Continue
        );
        assert_eq!(
            screen.handle_key(KeyCode::Char('q'), 1_000, &mut refresh),
            AssistantControl::Quit
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assistant_tui_switches_to_tier_editor_and_applies_auto_update_atomically() {
        let dir = std::env::temp_dir().join(format!("ramshared-aui-2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let au_path = dir.join("auto-update.conf");
        let mut screen = AssistantScreen::new(
            fixture_snapshot(),
            fixture_checklist(),
            dir.join("cascade.conf"),
            au_path.clone(),
        );
        let mut refresh = || Ok((fixture_snapshot(), fixture_checklist()));
        screen.handle_key(KeyCode::Char('u'), 1_000, &mut refresh);
        assert!(render_buffer(&screen).contains("Enable signed auto-update"));
        screen.handle_key(KeyCode::Enter, 1_000, &mut refresh);
        assert!(!au_path.exists());
        for c in "APPLY".chars() {
            screen.handle_key(KeyCode::Char(c), 1_000, &mut refresh);
        }
        screen.handle_key(KeyCode::Enter, 1_000, &mut refresh);
        assert!(
            std::fs::read_to_string(&au_path)
                .unwrap()
                .contains("enabled=1")
        );
        // Key input after done stays a no-op until Esc.
        screen.handle_key(KeyCode::Char('x'), 1_000, &mut refresh);
        screen.handle_key(KeyCode::Esc, 1_000, &mut refresh);
        // Toggle back off with backspace edit check.
        screen.handle_key(KeyCode::Char('u'), 1_000, &mut refresh);
        assert!(render_buffer(&screen).contains("Disable signed auto-update"));
        screen.handle_key(KeyCode::Char('X'), 1_000, &mut refresh);
        screen.handle_key(KeyCode::Backspace, 1_000, &mut refresh);
        screen.handle_key(KeyCode::Esc, 1_000, &mut refresh);
        assert_eq!(
            screen.handle_key(KeyCode::Char('e'), 1_000, &mut refresh),
            AssistantControl::OpenTierEditor
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assistant_tui_surfaces_symlink_write_refusals_refresh_errors_and_headless_wizard() {
        let dir = std::env::temp_dir().join(format!("ramshared-aui-3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        let cascade_link = dir.join("cascade.conf");
        let au_link = dir.join("auto-update.conf");
        std::os::unix::fs::symlink(&outside, &cascade_link).unwrap();
        std::os::unix::fs::symlink(&outside, &au_link).unwrap();

        let mut screen = AssistantScreen::new(
            fixture_snapshot(),
            fixture_checklist(),
            cascade_link,
            au_link,
        );
        let mut failing_refresh = || Err(InventoryError("probe busy".into()));
        // Refresh error surfaces in banner without crashing; unknown key ignored.
        screen.handle_key(KeyCode::Char('r'), 1_000, &mut failing_refresh);
        assert!(render_buffer(&screen).contains("Refresh failed: probe busy"));
        screen.handle_key(KeyCode::Char('z'), 1_000, &mut failing_refresh);

        // Symlinked cascade.conf write fails closed inside modal.
        screen.handle_key(KeyCode::Char('a'), 1_000, &mut failing_refresh);
        for c in "APPLY".chars() {
            screen.handle_key(KeyCode::Char(c), 1_000, &mut failing_refresh);
        }
        screen.handle_key(KeyCode::Enter, 1_000, &mut failing_refresh);
        assert!(render_buffer(&screen).contains("symlink"));
        screen.handle_key(KeyCode::Esc, 1_000, &mut failing_refresh);

        // Symlinked auto-update.conf write fails closed inside modal.
        screen.handle_key(KeyCode::Char('u'), 1_000, &mut failing_refresh);
        for c in "APPLY".chars() {
            screen.handle_key(KeyCode::Char(c), 1_000, &mut failing_refresh);
        }
        screen.handle_key(KeyCode::Enter, 1_000, &mut failing_refresh);
        assert!(render_buffer(&screen).contains("symlink"));
        screen.handle_key(KeyCode::Esc, 1_000, &mut failing_refresh);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");

        // Headless JSON and text modes + live constructor succeed without TTY.
        let mut json_out = Vec::new();
        run_wizard_with_tty(true, false, &mut json_out).unwrap();
        let json_val: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(json_val["items"].as_array().unwrap().len(), 6);
        let mut text_out = Vec::new();
        run_wizard_with_tty(false, false, &mut text_out).unwrap();
        assert!(
            String::from_utf8(text_out)
                .unwrap()
                .contains("Unified Setup & Configuration Assistant")
        );
        assert!(AssistantScreen::from_live().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
