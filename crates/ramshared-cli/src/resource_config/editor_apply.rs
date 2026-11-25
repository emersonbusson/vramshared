// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/editor_apply.rs
//! Confirmed modal dialog in `ramshared config` to apply bar-adjusted 3-tier
//! sizing to `/etc/ramshared/cascade.conf`.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::recommended::{render_cascade_conf, write_cascade_conf_atomic};
use super::tier_editor::Editor;
use super::{InventoryError, ResourceSnapshot, unix_millis};
use crate::cascade::CASCADE_CONF_FILE;

pub(super) struct ApplyDialog {
    target_path: PathBuf,
    confirmation: String,
    preview_body: Option<String>,
    message: String,
    done: bool,
}

impl ApplyDialog {
    pub(super) fn new_production(editor: &Editor, now: u64) -> Self {
        Self::at_path(PathBuf::from(CASCADE_CONF_FILE), editor, now)
    }

    pub(super) fn at_path(target_path: PathBuf, editor: &Editor, now: u64) -> Self {
        match Self::build_preview(editor, now) {
            Ok(body) => Self {
                target_path,
                confirmation: String::new(),
                preview_body: Some(body),
                message: String::new(),
                done: false,
            },
            Err(error) => Self {
                target_path,
                confirmation: String::new(),
                preview_body: None,
                message: error.to_string(),
                done: false,
            },
        }
    }

    fn build_preview(editor: &Editor, now: u64) -> Result<String, InventoryError> {
        let (zram_mib, vram_mib, headroom_mib) = editor.cascade_conf_values(now)?;
        render_cascade_conf(
            zram_mib,
            vram_mib,
            headroom_mib,
            &format!("ramshared config ({} preset)", editor.preset_label()),
        )
    }

    pub(super) fn input(
        &mut self,
        key: KeyCode,
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) {
        if self.done || self.preview_body.is_none() {
            return;
        }
        match key {
            KeyCode::Backspace => {
                self.confirmation.pop();
            }
            KeyCode::Char(c) if !c.is_control() && self.confirmation.len() < 5 => {
                self.confirmation.push(c);
            }
            KeyCode::Enter => {
                if self.confirmation != "APPLY" {
                    self.message = "type APPLY exactly, or Esc to cancel".into();
                    return;
                }
                match self.commit(editor, observe) {
                    Ok(()) => {}
                    Err(error) => self.message = error.to_string(),
                }
            }
            _ => {}
        }
    }

    fn commit(
        &mut self,
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) -> Result<(), InventoryError> {
        let snapshot = observe()?;
        let now = unix_millis();
        editor.reconcile(Editor::from_snapshot(&snapshot, now));
        let fresh_body = Self::build_preview(editor, now)?;
        if self.preview_body.as_deref() != Some(fresh_body.as_str()) {
            self.preview_body = Some(fresh_body);
            self.confirmation.clear();
            return Err(InventoryError(
                "limits changed since preview; review and type APPLY again".into(),
            ));
        }
        write_cascade_conf_atomic(&self.target_path, &fresh_body, true).map_err(|err| {
            InventoryError(format!(
                "{err} (if permission denied, run `sudo ramshared config` or press `s` to save a user draft)"
            ))
        })?;
        self.done = true;
        self.message = format!(
            "Saved 3-tier policy to {}. Live swap tiers were not disrupted; apply takes effect on next `ramshared up` or safe reload. Esc returns.",
            self.target_path.display()
        );
        Ok(())
    }

    pub(super) fn target_path(&self) -> &Path {
        &self.target_path
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(area);
        frame.render_widget(Clear, area);
        let body = if let Some(preview) = &self.preview_body {
            format!(
                "Target: {}\nAll 3 tiers remain enabled in writeback posture (ZRAM -> Origin/VRAM -> Fallback).\n\n{preview}",
                self.target_path().display()
            )
        } else {
            format!(
                "Cannot prepare {} from current bar values.\nEsc returns to the editor.",
                self.target_path().display()
            )
        };
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Apply 3-tier cascade configuration"),
            ),
            rows[0],
        );
        let prompt = if self.done || self.preview_body.is_none() {
            String::new()
        } else {
            format!("Type APPLY: {}  (Esc cancel)", self.confirmation)
        };
        frame.render_widget(
            Paragraph::new(format!("{prompt}\n{}", self.message))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL)),
            rows[1],
        );
    }
}

#[cfg(test)]
mod tests;
