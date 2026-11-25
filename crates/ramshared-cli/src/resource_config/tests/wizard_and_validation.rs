// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tests/wizard_and_validation.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn render_ui_prepares_interactive_session_draws_save_dialog_and_formats_warnings_caps() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    assert_eq!(render_ui::format_cap_mib(Some(1024)), "1024 bytes");

    let mut snapshot = fixture_snapshot();
    let mut unknown_dev = fixture_block_device();
    unknown_dev.rotational = None;
    snapshot.block_devices.push(unknown_dev);
    snapshot.warnings.push("sample advisory warning".into());

    let text = render_text(&snapshot);
    assert!(text.contains("rotation unknown"), "{text}");
    assert!(text.contains("Warning: sample advisory warning"), "{text}");

    let root = draft_test_directory("render-ui-session");
    let profile_path = root.join("profile.toml");
    let now = unix_millis();
    let mut editor = tier_editor::Editor::from_snapshot(&snapshot, now);
    editor.next();
    editor.fill_selected(now);
    let profile_toml = editor
        .profile(&snapshot, now)
        .expect("selected cap profile should serialize")
        .to_toml()
        .expect("profile TOML formatting should succeed");
    fs::write(&profile_path, &profile_toml).expect("write test profile fixture");

    let missing_err = render_ui::prepare_interactive_session(
        Some(&root.join("missing.toml").to_string_lossy()),
        now,
        &mut || Ok(snapshot.clone()),
    )
    .expect_err("missing profile path must refuse");
    assert!(missing_err.0.contains("does not exist"));

    let (loaded_snap, loaded_editor) = render_ui::prepare_interactive_session(
        Some(&profile_path.to_string_lossy()),
        now,
        &mut || Ok(snapshot.clone()),
    )
    .expect("valid profile should initialize editor");
    assert_eq!(loaded_snap.platform, RuntimePlatform::Wsl2);

    let (default_snap, default_editor) =
        render_ui::prepare_interactive_session(None, now, &mut || Ok(snapshot.clone()))
            .expect("none profile should initialize default editor");
    assert_eq!(default_editor.preset_label(), "recommended");
    assert_eq!(default_editor.cascade_conf_values(now).unwrap().0, 4096);

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut dialog = Some(render_ui::ActiveModal::Save(
        editor_save::SaveDialog::default(),
    ));
    terminal
        .draw(|frame| {
            render_ui::draw_interactive_state(frame, &default_snap, 0, &default_editor, None);
        })
        .unwrap();
    terminal
        .draw(|frame| {
            render_ui::draw_interactive_state(
                frame,
                &default_snap,
                0,
                &default_editor,
                dialog.as_ref(),
            );
        })
        .unwrap();
    let apply_modal = Some(render_ui::ActiveModal::Apply(
        editor_apply::ApplyDialog::new_production(&default_editor, now),
    ));
    terminal
        .draw(|frame| {
            render_ui::draw_interactive_state(
                frame,
                &default_snap,
                0,
                &default_editor,
                apply_modal.as_ref(),
            );
        })
        .unwrap();

    // Drive SaveDialog through handle_interactive_key including Enter -> observe callback
    let mut work_snap = loaded_snap.clone();
    let mut work_editor = loaded_editor;
    let mut scroll = 0u16;
    let draft_dest = root
        .join("interactive-draft.toml")
        .to_string_lossy()
        .into_owned();
    for ch in draft_dest.chars() {
        let ctrl = render_ui::handle_interactive_key(
            KeyCode::Char(ch),
            &mut work_snap,
            &mut scroll,
            &mut work_editor,
            &mut dialog,
            now,
            &mut || Ok(snapshot.clone()),
        )
        .unwrap();
        assert_eq!(ctrl, render_ui::EditorLoopControl::Continue);
    }
    let mut observed = false;
    let ctrl = render_ui::handle_interactive_key(
        KeyCode::Enter,
        &mut work_snap,
        &mut scroll,
        &mut work_editor,
        &mut dialog,
        now,
        &mut || {
            observed = true;
            Ok(snapshot.clone())
        },
    )
    .unwrap();
    assert_eq!(ctrl, render_ui::EditorLoopControl::Continue);
    assert!(
        observed,
        "SaveDialog Enter must invoke the observe callback"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn render_ui_renders_wsl2_variants_config_frame_and_interactive_key_actions() {
    let mut snapshot = fixture_snapshot();
    let now = unix_millis();
    // Cover WSL2 volume eligibility branches (valid canonical, missing canonical letter, None windows, empty windows)
    let wsl_text = render_text(&snapshot);
    assert!(wsl_text.contains("Windows volumes:"));
    assert!(wsl_text.contains("eligible volume candidate"));

    let mut no_windows = snapshot.clone();
    no_windows.windows = None;
    assert!(render_text(&no_windows).contains("unavailable"));

    let mut empty_windows = snapshot.clone();
    if let Some(w) = empty_windows.windows.as_mut() {
        w.volumes.clear();
    }
    assert!(render_text(&empty_windows).contains("none reported"));

    // Cover draw_config_frame directly via TestBackend
    let mut editor = tier_editor::Editor::from_snapshot(&snapshot, now);
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_config_frame(frame, &snapshot, 0, &editor))
        .unwrap();

    // Cover handle_interactive_key transitions without needing a TTY
    let mut scroll = 0u16;
    let mut save_dialog = None;
    let refresh_count = Rc::new(Cell::new(0usize));
    let snapshot_clone = snapshot.clone();
    let mut collect_fn = {
        let count = Rc::clone(&refresh_count);
        move || {
            count.set(count.get() + 1);
            Ok(snapshot_clone.clone())
        }
    };
    assert_eq!(
        handle_interactive_key(
            KeyCode::Down,
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert_eq!(scroll, 1);
    assert_eq!(
        handle_interactive_key(
            KeyCode::Up,
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert_eq!(scroll, 0);
    for code in [
        KeyCode::Tab,
        KeyCode::Right,
        KeyCode::Left,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Char('v'),
        KeyCode::Char('p'),
        KeyCode::Char('r'),
        KeyCode::Char('x'),
        KeyCode::Char('s'),
    ] {
        assert_eq!(
            handle_interactive_key(
                code,
                &mut snapshot,
                &mut scroll,
                &mut editor,
                &mut save_dialog,
                now,
                &mut collect_fn
            )
            .unwrap(),
            EditorLoopControl::Continue
        );
    }
    assert_eq!(refresh_count.get(), 1);
    assert!(save_dialog.is_some());
    assert_eq!(
        handle_interactive_key(
            KeyCode::Char('a'),
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert_eq!(
        handle_interactive_key(
            KeyCode::Esc,
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert!(save_dialog.is_none());
    // Pressing 'a' opens ApplyDialog, accepts key input, and closes on Esc.
    assert_eq!(
        handle_interactive_key(
            KeyCode::Char('a'),
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert!(matches!(
        save_dialog,
        Some(render_ui::ActiveModal::Apply(_))
    ));
    assert_eq!(
        handle_interactive_key(
            KeyCode::Char('A'),
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert_eq!(
        handle_interactive_key(
            KeyCode::Esc,
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Continue
    );
    assert!(save_dialog.is_none());
    assert_eq!(
        handle_interactive_key(
            KeyCode::Char('q'),
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Break
    );
    assert_eq!(
        handle_interactive_key(
            KeyCode::Esc,
            &mut snapshot,
            &mut scroll,
            &mut editor,
            &mut save_dialog,
            now,
            &mut collect_fn
        )
        .unwrap(),
        EditorLoopControl::Break
    );
}
