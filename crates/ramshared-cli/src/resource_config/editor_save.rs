// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/editor_save.rs
//! Reviewed editor draft persistence through the existing exclusive writer.

use super::{InventoryError, ResourceSnapshot, tier_editor::Editor};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::path::Path;

#[derive(Default)]
pub(super) struct SaveDialog {
    path: String,
    confirmation: String,
    reviewed: Option<(String, String)>,
    message: String,
    done: bool,
    scroll: u16,
}

impl SaveDialog {
    pub(super) fn input(
        &mut self,
        key: KeyCode,
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) {
        if self.done {
            return;
        }
        match key {
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Backspace => {
                self.field().pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                let limit = if self.reviewed.is_some() { 4 } else { 4096 };
                if self.field().len() + c.len_utf8() <= limit {
                    self.field().push(c);
                }
            }
            KeyCode::Enter => {
                let result = if self.reviewed.is_none() {
                    self.prepare(editor, observe)
                } else if self.confirmation == "SAVE" {
                    self.save(editor, observe)
                } else {
                    Err(InventoryError("type SAVE exactly, or Esc to cancel".into()))
                };
                if let Err(error) = result {
                    self.message = error.to_string();
                }
            }
            _ => {}
        }
    }

    fn field(&mut self) -> &mut String {
        if self.reviewed.is_some() {
            &mut self.confirmation
        } else {
            &mut self.path
        }
    }

    fn current_profile(
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) -> Result<(String, String), InventoryError> {
        let snapshot = observe()?;
        let now = super::unix_millis();
        editor.reconcile(Editor::from_snapshot(&snapshot, now));
        let profile = editor.profile(&snapshot, now)?;
        if profile.targets.is_empty() && profile.planned_caps == super::PlannedTierCaps::default() {
            return Err(InventoryError(
                "select at least one desired tier value before saving".into(),
            ));
        }
        let text = profile
            .to_toml()
            .map_err(|error| InventoryError(error.to_string()))?;
        let plan = super::build_resource_plan(&snapshot, Some(&text))?;
        if plan.status == "blocked" || plan.apply_enabled || plan.writes_performed {
            return Err(InventoryError(
                "current profile cannot be saved; review resource limits".into(),
            ));
        }
        Ok((text, super::render_plan_text(&plan)))
    }

    fn prepare(
        &mut self,
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) -> Result<(), InventoryError> {
        if self.path.trim().is_empty() {
            return Err(InventoryError("enter a new draft file path".into()));
        }
        super::resolve_user_draft_path(Path::new(&self.path))?;
        self.reviewed = Some(Self::current_profile(editor, observe)?);
        self.confirmation.clear();
        self.message.clear();
        self.scroll = 0;
        Ok(())
    }

    fn save(
        &mut self,
        editor: &mut Editor,
        observe: &mut dyn FnMut() -> Result<ResourceSnapshot, InventoryError>,
    ) -> Result<(), InventoryError> {
        let (text, _) = match Self::current_profile(editor, observe) {
            Ok(current) => current,
            Err(error) => {
                self.reviewed = None;
                self.confirmation.clear();
                return Err(error);
            }
        };
        if self
            .reviewed
            .as_ref()
            .is_none_or(|(reviewed, _)| reviewed != &text)
        {
            self.reviewed = None;
            self.confirmation.clear();
            return Err(InventoryError(
                "profile changed since review; review again before saving".into(),
            ));
        }
        let saved = super::save_profile_draft(Path::new(&self.path), &text)?;
        self.message = format!(
            "Desired draft saved at {}. No tier changes were applied. Esc returns to the editor.",
            saved.display()
        );
        self.done = true;
        Ok(())
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(area);
        frame.render_widget(Clear, area);
        let body = if let Some((_, plan)) = &self.reviewed {
            format!(
                "Draft file: {}\nReview desired values; no tier changes will be applied. GPU estimates require runtime driver admission.\n\n{plan}",
                self.path
            )
        } else {
            "Choose a new user-owned draft file. Existing files are never replaced.\nEsc cancels.\nSaving records desired ceilings only; GPU estimates are not driver-budget admission.".into()
        };
        frame.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Save desired draft — not applied"),
                ),
            rows[0],
        );
        let prompt = if self.done {
            String::new()
        } else if self.reviewed.is_some() {
            format!("Type SAVE: {}  (↑/↓ review, Esc cancel)", self.confirmation)
        } else {
            format!("New draft path: {}", self.path)
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
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn reviewed_editor_save_refuses_overwrite_and_changed_values() {
        let snapshot = super::super::tests::fixture_snapshot();
        let mut editor = Editor::from_snapshot(&snapshot, super::super::unix_millis());
        editor.adjust_selected(true, super::super::unix_millis());
        let dir = super::super::tests::draft_test_directory("bar-save");
        let path = dir.join("draft.toml");
        let mut dialog = SaveDialog {
            path: path.to_string_lossy().into_owned(),
            ..SaveDialog::default()
        };
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.reviewed.is_some());
        assert!(!path.exists());
        dialog.confirmation = "SAVE".into();
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.done);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).expect("terminal");
        terminal
            .draw(|frame| dialog.draw(frame))
            .expect("saved state");
        dialog.input(KeyCode::Enter, &mut editor, &mut || {
            panic!("saved dialog must not observe or write again")
        });
        let before = std::fs::read(&path).expect("saved profile");
        let mut retry = SaveDialog {
            path: dialog.path.clone(),
            ..SaveDialog::default()
        };
        retry.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        retry.confirmation = "SAVE".into();
        retry.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(!retry.done);
        assert_eq!(std::fs::read(&path).expect("original"), before);
        let mut changed = SaveDialog {
            path: dir.join("changed.toml").to_string_lossy().into_owned(),
            ..SaveDialog::default()
        };
        changed.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        editor.adjust_selected(true, super::super::unix_millis());
        changed.confirmation = "SAVE".into();
        changed.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(changed.reviewed.is_none() && !changed.done);
        assert!(!dir.join("changed.toml").exists());
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }
    #[test]
    fn dialog_refusals_and_rendering_never_create_a_draft() {
        let snapshot = super::super::tests::fixture_snapshot();
        let now = super::super::unix_millis();
        let mut editor = Editor::from_snapshot(&snapshot, now);
        let mut dialog = SaveDialog::default();
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.reviewed.is_none());
        dialog.input(
            KeyCode::Char('x'),
            &mut editor,
            &mut || Ok(snapshot.clone()),
        );
        dialog.input(
            KeyCode::Backspace,
            &mut editor,
            &mut || Ok(snapshot.clone()),
        );
        dialog.input(KeyCode::Down, &mut editor, &mut || Ok(snapshot.clone()));
        dialog.input(KeyCode::Up, &mut editor, &mut || Ok(snapshot.clone()));
        let dir = super::super::tests::draft_test_directory("dialog-refusal");
        dialog.path = dir.join("empty.toml").to_string_lossy().into_owned();
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.reviewed.is_none());
        editor.adjust_selected(true, now);
        dialog.input(KeyCode::Enter, &mut editor, &mut || {
            Err(InventoryError("observation failed".into()))
        });
        assert!(dialog.reviewed.is_none());
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.reviewed.is_some());
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.message.contains("SAVE exactly"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|frame| dialog.draw(frame))
            .expect("review screen");
        for c in "SAVE".chars() {
            dialog.input(KeyCode::Char(c), &mut editor, &mut || Ok(snapshot.clone()));
        }
        dialog.input(KeyCode::Enter, &mut editor, &mut || {
            Err(InventoryError("fresh observation failed".into()))
        });
        assert!(dialog.reviewed.is_none() && !dialog.done);
        terminal
            .draw(|frame| dialog.draw(frame))
            .expect("path screen");
        assert!(!dir.join("empty.toml").exists());
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }
    #[test]
    fn storage_save_refuses_capacity_loss_after_review() {
        let snapshot = super::super::tests::fixture_snapshot();
        let now = super::super::unix_millis();
        let mut editor = Editor::from_snapshot(&snapshot, now);
        editor.next();
        editor.adjust_selected(true, now);
        let dir = super::super::tests::draft_test_directory("storage-save");
        let path = dir.join("storage.toml");
        let mut dialog = SaveDialog {
            path: path.to_string_lossy().into_owned(),
            ..SaveDialog::default()
        };
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.reviewed.is_some());
        let mut reduced = snapshot.clone();
        reduced.windows.as_mut().expect("host fixture").volumes[0].free_bytes =
            Some(10 * 1024 * 1024 * 1024);
        dialog.confirmation = "SAVE".into();
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(reduced.clone()));
        assert!(!path.exists() && dialog.reviewed.is_none());
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        dialog.confirmation = "SAVE".into();
        dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
        assert!(dialog.done && path.is_file());
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }

    #[test]
    fn modal_input_is_bounded_and_unsupported_keys_do_not_change_it() {
        let snapshot = super::super::tests::fixture_snapshot();
        let mut editor = Editor::from_snapshot(&snapshot, super::super::unix_millis());
        let mut dialog = SaveDialog {
            path: "x".repeat(4096),
            ..SaveDialog::default()
        };
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('é'),
            KeyCode::Char('\0'),
            KeyCode::F(1),
        ] {
            dialog.input(code, &mut editor, &mut || panic!("typing must not observe"));
        }
        assert_eq!(dialog.path.len(), 4096);
        dialog.path = "/nonexistent-ramshared-dialog-parent/profile.toml".into();
        dialog.input(KeyCode::Enter, &mut editor, &mut || {
            panic!("invalid path must be refused before observation")
        });
        assert!(dialog.reviewed.is_none());
        dialog.reviewed = Some((String::new(), String::new()));
        dialog.confirmation = "SAVE".into();
        dialog.input(KeyCode::Char('x'), &mut editor, &mut || {
            panic!("typing must not observe")
        });
        assert_eq!(dialog.confirmation, "SAVE");
    }
}
