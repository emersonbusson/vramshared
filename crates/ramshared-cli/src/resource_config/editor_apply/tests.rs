// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/editor_apply/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::resource_config::gpu_inventory;

#[test]
fn apply_dialog_requires_exact_confirmation_and_writes_atomically() {
    let mut snapshot = crate::resource_config::tests::fixture_snapshot();
    let now = unix_millis();
    snapshot.gpu_inventory = gpu_inventory::parse_inventory("GPU-a, 6144, 6144").unwrap();
    snapshot.gpu_observed_unix_ms = now;

    let mut editor = Editor::from_snapshot_recommended(&snapshot, now);
    let dir = std::env::temp_dir().join(format!("ramshared-apply-dlg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("cascade.conf");

    let mut dialog = ApplyDialog::at_path(target.clone(), &editor, now);
    assert!(dialog.preview_body.is_some());

    // Wrong confirmation refused without writing.
    dialog.input(
        KeyCode::Char('N'),
        &mut editor,
        &mut || Ok(snapshot.clone()),
    );
    dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
    assert!(dialog.message.contains("APPLY exactly"));
    assert!(!target.exists());
    dialog.input(
        KeyCode::Backspace,
        &mut editor,
        &mut || Ok(snapshot.clone()),
    );

    // Render preview screen.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| dialog.draw(frame)).unwrap();

    // Exact APPLY writes validated cascade.conf.
    for ch in "APPLY".chars() {
        dialog.input(KeyCode::Char(ch), &mut editor, &mut || Ok(snapshot.clone()));
    }
    dialog.input(KeyCode::Enter, &mut editor, &mut || Ok(snapshot.clone()));
    assert!(dialog.done);
    let body = std::fs::read_to_string(&target).unwrap();
    assert!(body.contains("ZRAM_MIB=4096"));
    assert!(body.contains("VRAM_MIB=4096"));
    assert!(body.contains("MIN_VRAM_HEADROOM_MIB=2048"));

    // Post-commit draw & no-op input.
    terminal.draw(|frame| dialog.draw(frame)).unwrap();
    dialog.input(KeyCode::Enter, &mut editor, &mut || {
        panic!("completed dialog must not observe again")
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_dialog_handles_invalid_editor_state_and_changed_values() {
    let snapshot = crate::resource_config::tests::fixture_snapshot();
    let now = unix_millis();
    let mut empty_editor = Editor::from_snapshot(&snapshot, now);
    let mut bad = ApplyDialog::new_production(&empty_editor, now);
    assert!(bad.preview_body.is_none());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| bad.draw(frame)).unwrap();
    bad.input(KeyCode::Enter, &mut empty_editor, &mut || {
        panic!("unready dialog must ignore input")
    });

    // If values change between preview and APPLY, commit asks to re-confirm.
    let mut rec_editor = Editor::from_snapshot_recommended(&snapshot, now);
    let dir = std::env::temp_dir().join(format!("ramshared-apply-chg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("cascade.conf");
    let mut dlg = ApplyDialog::at_path(target.clone(), &rec_editor, now);
    rec_editor.adjust_selected(false, now);
    for ch in "APPLY".chars() {
        dlg.input(KeyCode::Char(ch), &mut rec_editor, &mut || {
            Ok(snapshot.clone())
        });
    }
    dlg.input(
        KeyCode::Enter,
        &mut rec_editor,
        &mut || Ok(snapshot.clone()),
    );
    assert!(!dlg.done);
    assert!(dlg.message.contains("changed since preview"));
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
