use super::*;
use fastab_remote_ipc::figterm::{EditBuffer, FigtermSession};

fn row(name: &str, kind: &str) -> SuggestionItem {
    SuggestionItem {
        name: name.into(),
        kind: kind.into(),
        ..Default::default()
    }
}

#[gpui::test]
fn presentation_refresh_preserves_temporary_and_effective_fuzzy_modes(cx: &mut gpui::TestAppContext) {
    for saved_fuzzy in [true, false] {
        let settings = fastab_settings::Settings::new_fake();
        settings.set_value("autocomplete.fuzzySearch", saved_fuzzy).unwrap();
        let _settings = fastab_settings::settings::install_override(settings.clone());
        let mut completion_settings = crate::settings_refresh::CompletionSettings::capture(&settings);
        let specs = tempfile::tempdir().unwrap();
        let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
        let (proxy, _events) = crate::event_loop::channel();
        let figterm = Arc::new(FigtermState::new());
        let platform = Arc::new(PlatformState::new(proxy.clone()));
        let session = Uuid::new_v4();
        let mut controller = cx.update(|cx| {
            let mut controller =
                OverlayController::start(cx, engine.clone(), proxy, figterm.clone(), platform).unwrap();
            controller.set_session(session);
            controller.state.update(cx, |state, _| {
                state.set_suggestions(vec![row("status", "subcommand")], "st".into());
            });
            // No last_input: toggling must not run a worker completion or
            // read/write the user's command history in this regression.
            controller
        });
        cx.update(|cx| {
            controller.handle_action("toggleFuzzySearch", session, &figterm, cx);
            let state = controller.state.read(cx);
            assert_eq!(state.fuzzy_search, !saved_fuzzy);
            assert_eq!(state.effective_fuzzy_search, !saved_fuzzy);
        });

        for (key, value) in [
            ("autocomplete.width", serde_json::json!(480)),
            ("autocomplete.fontSize", serde_json::json!(14)),
            ("dashboard.language", serde_json::json!("zh-CN")),
        ] {
            settings.set_value(key, value).unwrap();
            assert!(
                !completion_settings.refresh(&settings),
                "{key} must not regenerate rows"
            );
            cx.update(|cx| {
                for _ in 0..2 {
                    // Apply the direct change and its file-watcher echo.
                    controller.apply_theme(cx);
                    let state = controller.state.read(cx);
                    assert_eq!(state.fuzzy_search, !saved_fuzzy, "{key}");
                    assert_eq!(state.effective_fuzzy_search, !saved_fuzzy, "{key}");
                }
            });
        }

        cx.update(|cx| {
            let generation = controller.generation.load(Ordering::Relaxed);
            // A spec can override matching independently of the user's
            // temporary mode. Exercise the actual result application path.
            controller.apply_completion(
                generation,
                Ok(CompleteResult {
                    fuzzy: saved_fuzzy,
                    ..Default::default()
                }),
                session,
                "",
                cx,
            );
            assert_eq!(controller.state.read(cx).fuzzy_search, !saved_fuzzy);
            assert_eq!(controller.state.read(cx).effective_fuzzy_search, saved_fuzzy);
        });
        settings.set_value("autocomplete.width", 500).unwrap();
        assert!(!completion_settings.refresh(&settings));
        cx.update(|cx| {
            for _ in 0..2 {
                controller.apply_theme(cx);
                let state = controller.state.read(cx);
                assert_eq!(state.fuzzy_search, !saved_fuzzy);
                assert_eq!(state.effective_fuzzy_search, saved_fuzzy);
            }
            // Return to the original mode through the real action before
            // saving the opposite default, so both fields must change.
            controller.state.update(cx, |state, _| {
                state.set_suggestions(vec![row("status", "subcommand")], "st".into());
            });
            controller.handle_action("toggleFuzzySearch", session, &figterm, cx);
            assert_eq!(controller.state.read(cx).fuzzy_search, saved_fuzzy);
        });
        assert_eq!(
            settings.get_bool("autocomplete.fuzzySearch").unwrap(),
            Some(saved_fuzzy)
        );
        settings.set_value("autocomplete.fuzzySearch", !saved_fuzzy).unwrap();
        assert!(
            completion_settings.refresh(&settings),
            "a changed saved default requests recompletion"
        );
        assert!(
            !completion_settings.refresh(&settings),
            "the watcher echo must not request it twice"
        );
        cx.update(|cx| {
            for _ in 0..2 {
                controller.apply_theme(cx);
                let state = controller.state.read(cx);
                assert_eq!(state.fuzzy_search, !saved_fuzzy);
                assert_eq!(state.effective_fuzzy_search, !saved_fuzzy);
            }
        });
        assert_eq!(
            futures::executor::block_on(engine.diagnostics())
                .unwrap()
                .requests
                .submitted,
            0
        );
    }
}

fn insertions(commands: &mut fastab_remote_ipc::outbox::Outbox) -> Vec<(Option<String>, Option<i64>, Option<bool>)> {
    let mut inserted = Vec::new();
    while let Some(frame) = commands.try_recv() {
        let (_, message) = fastab_proto::FigMessage::parse(&mut frame.bytes()).unwrap();
        let message = message.decode::<fastab_proto::remote::Clientbound>().unwrap();
        if let Some(fastab_proto::remote::clientbound::Packet::Request(request)) = message.packet {
            if let Some(fastab_proto::remote::clientbound::request::Request::InsertText(insert)) = request.request {
                inserted.push((
                    insert.insertion,
                    insert.deletion.map(|value| value as i64),
                    insert.immediate,
                ));
            }
        }
    }
    inserted
}

#[gpui::test]
fn hidden_payload_retires_then_show_requests_fresh_caret(cx: &mut gpui::TestAppContext) {
    // The real worker reads process-wide settings on its own thread. Isolate
    // this override so the regression never loads the user's shell history.
    const CHILD: &str = "FASTAB_SHOW_CARET_REGRESSION_CHILD";
    const TEST: &str = "overlay::tab_tests::hidden_payload_retires_then_show_requests_fresh_caret";
    if std::env::var(CHILD).as_deref() != Ok(TEST) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, TEST)
            .output()
            .expect("isolated caret regression process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use fastab_settings::JsonStore as _;
    *fastab_settings::OldSettings::data_lock().write() = Some(
        [("autocomplete.history.disableLoading".into(), serde_json::json!(true))]
            .into_iter()
            .collect(),
    );
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[
        ("beta.history.mode", serde_json::json!("off")),
        ("autocomplete.onlyShowOnTab", serde_json::json!(true)),
    ]));
    let specs = tempfile::tempdir().unwrap();
    std::fs::write(
        specs.path().join("fastab-show-fixture.json"),
        r#"{"names":["fastab-show-fixture"],"subcommands":[{"names":["choice-a"]},{"names":["choice-b"]}]}"#,
    )
    .unwrap();
    let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
    let (proxy, events) = crate::event_loop::channel();
    let figterm = Arc::new(FigtermState::new());
    let platform = Arc::new(PlatformState::new(proxy.clone()));
    let session = Uuid::new_v4();
    let buffer = "fastab-show-fixture ch";
    let mut controller = cx.update(|cx| {
        let mut controller =
            OverlayController::start(cx, engine.clone(), proxy, figterm.clone(), platform.clone()).unwrap();
        controller.enabled = true;
        controller.complete_buffer(
            buffer.into(),
            String::new(),
            buffer.len() as u32,
            session,
            figterm.clone(),
            cx,
        );
        controller.state.update(cx, |state, _| {
            state.set_suggestions(
                vec![row("choice-a", "subcommand"), row("choice-b", "subcommand")],
                "ch".into(),
            );
            state.current_arg_description = String::with_capacity(4096);
        });
        controller.hide_until_shown(cx);
        controller
    });
    cx.run_until_parked();
    for elapsed in [6, 3] {
        cx.executor().advance_clock(Duration::from_secs(elapsed));
        cx.run_until_parked();
        cx.update(|cx| {
            // Hidden updates must not extend the original retirement deadline.
            controller.hide_until_shown(cx);
            let state = controller.state.read(cx);
            assert!(!state.visible && state.suppress_until_shown);
            assert_eq!(state.items.len(), 2);
            assert!(state.current_arg_description.capacity() >= 4096);
            assert!(controller.handle.lock().unwrap().is_none());
            assert!(matches!(controller.completion_display, CompletionDisplayState::Idle));
        });
        cx.run_until_parked();
    }
    let waiting = futures::executor::block_on(engine.diagnostics()).unwrap();
    assert_eq!(waiting.requests.submitted, 0);
    assert!(waiting.engine.is_none(), "waiting for a caret must not load specs");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    cx.update(|cx| {
        let state = controller.state.read(cx);
        assert_eq!(
            state.current_arg_description.capacity(),
            0,
            "idle retirement still ran at ten seconds"
        );
        assert!(state.items.is_empty(), "cold Show recovery regenerates rows");
        assert!(!state.visible);
        assert!(controller.handle.lock().unwrap().is_none());
        controller.handle_action("showAutocomplete", session, &figterm, cx);
        assert!(controller.wants_caret(cx));
        let position = WindowPosition::Absolute(Position::Logical(LogicalPosition::new(100.0, 100.0)));
        controller.apply_position(position, &platform, cx);
        controller.apply_position(position, &platform, cx);
        assert!(!controller.state.read(cx).suppress_until_shown);
    });
    // TestAppContext cannot open a native NSWindow. Verify recovery through
    // the actual worker and host event before the visible result is applied.
    assert_eq!(
        futures::executor::block_on(engine.diagnostics())
            .unwrap()
            .requests
            .completed,
        1
    );
    cx.run_until_parked();
    let result = events
        .try_iter()
        .find_map(|event| match event {
            Event::GpuiOverlayComplete { result, session_id, .. } if session_id == session => Some(result.unwrap()),
            _ => None,
        })
        .expect("a valid caret must resume the requested completion");
    assert!(result.suggestions.iter().any(|item| item.name == "choice-a"));
    assert!(result.suggestions.iter().any(|item| item.name == "choice-b"));
}

#[gpui::test]
fn default_tab_preserves_action_guards_and_normal_completion(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let specs = tempfile::tempdir().unwrap();
    let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
    let (proxy, _events) = crate::event_loop::channel();
    let figterm = Arc::new(FigtermState::new());
    let platform = Arc::new(PlatformState::new(proxy.clone()));
    let session = Uuid::new_v4();
    let (sender, mut commands) = fastab_remote_ipc::outbox::channel();
    // Capture the real controller's output without connecting to a user's PTY.
    figterm.insert(FigtermSession {
        id: session,
        secret: String::new(),
        sender,
        dead_since: None,
        edit_buffer: EditBuffer::default(),
        last_receive: tokio::time::Instant::now(),
        context: None,
        flattened_env: Arc::new(Vec::new()),
        terminal_cursor_coordinates: None,
        current_session_metrics: None,
        intercept: InterceptMode::Unlocked,
        intercept_global: InterceptMode::Unlocked,
    });
    let mut controller = cx.update(|cx| {
        let mut controller = OverlayController::start(cx, engine, proxy, figterm.clone(), platform).unwrap();
        controller.set_session(session);
        // No last_input: these acceptance checks must not write recency data.
        controller
    });
    let tab = DEFAULT_OVERLAY_BINDINGS
        .iter()
        .find(|(_, bindings)| bindings.contains(&"tab"))
        .unwrap()
        .0;
    let enter = DEFAULT_OVERLAY_BINDINGS
        .iter()
        .find(|(_, bindings)| bindings.contains(&"enter"))
        .unwrap()
        .0;
    let show = |controller: &mut OverlayController, rows, search: &str, selected, cx: &mut App| {
        controller.state.update(cx, |state, _| {
            state.suppress_until_shown = false;
            state.set_suggestions(rows, search.into());
            state.selected = selected;
        });
    };
    let newline = vec![(Some("\n".into()), Some(0), Some(false))];
    for kind in ["auto-execute", "special"] {
        let action = SuggestionItem {
            insert_value: Some("\n".into()),
            ..row("status", kind)
        };
        // The exact-command result contains both the execution row and the
        // ordinary row. Also cover an action selected away from index zero.
        for selected in [0, 1] {
            let mut rows = vec![action.clone(), row("status", "subcommand")];
            rows.swap(0, selected);
            cx.update(|cx| {
                show(&mut controller, rows, "status", selected, cx);
                controller.handle_action(tab, session, &figterm, cx);
                assert!(
                    insertions(&mut commands).is_empty(),
                    "Tab accepted {kind} at {selected}"
                );
                let state = controller.state.read(cx);
                assert!(state.visible);
                assert_eq!(state.items.len(), 2);
                assert_eq!(state.selected, selected);
                controller.handle_action(enter, session, &figterm, cx);
                assert_eq!(insertions(&mut commands), newline, "Enter must still accept {kind}");
            });
        }
        cx.update(|cx| {
            // Preserve the existing sole-action Full acceptance contract.
            show(&mut controller, vec![action], "status", 0, cx);
            controller.handle_action(tab, session, &figterm, cx);
            assert_eq!(insertions(&mut commands), newline, "sole {kind} must be accepted once");
        });
    }
    for (search, expected) in [("ch", "e"), ("che", "ckout")] {
        cx.update(|cx| {
            // An unrelated action row must not disable an ordinary selection.
            show(
                &mut controller,
                vec![
                    row("run", "auto-execute"),
                    row("checkout", "subcommand"),
                    row("cherry", "subcommand"),
                ],
                search,
                1,
                cx,
            );
            controller.handle_action(tab, session, &figterm, cx);
            assert_eq!(
                insertions(&mut commands),
                vec![(Some(expected.into()), Some(0), Some(false))]
            );
        });
    }
    cx.update(|cx| {
        show(&mut controller, Vec::new(), "", 0, cx);
        controller.handle_action(tab, session, &figterm, cx);
        assert!(insertions(&mut commands).is_empty());
        show(&mut controller, vec![row("status", "subcommand")], "status", 0, cx);
        controller.handle_action("execute", session, &figterm, cx);
        assert_eq!(insertions(&mut commands), newline, "explicit execution stays available");
    });
}

#[gpui::test]
fn rejected_insert_clears_prediction_and_does_not_advance_intercept_state(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let specs = tempfile::tempdir().unwrap();
    let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
    let (proxy, _events) = crate::event_loop::channel();
    let figterm = Arc::new(FigtermState::new());
    let platform = Arc::new(PlatformState::new(proxy.clone()));
    let session = Uuid::new_v4();
    let (sender, receiver) = fastab_remote_ipc::outbox::channel();
    figterm.insert(FigtermSession {
        id: session,
        secret: String::new(),
        sender,
        dead_since: None,
        edit_buffer: EditBuffer::default(),
        last_receive: tokio::time::Instant::now(),
        context: None,
        flattened_env: Arc::new(Vec::new()),
        terminal_cursor_coordinates: None,
        current_session_metrics: None,
        intercept: InterceptMode::Unlocked,
        intercept_global: InterceptMode::Unlocked,
    });
    drop(receiver);
    cx.update(|cx| {
        let mut controller = OverlayController::start(cx, engine, proxy, figterm.clone(), platform).unwrap();
        controller.set_session(session);
        *controller.self_insertion.lock().unwrap() = Some("old prediction".into());
        assert!(!controller.insert_text("x", 0, false, &figterm, cx));
        assert!(controller.self_insertion.lock().unwrap().is_none());
        set_intercept_flags(&figterm, session, true, true);
        let session = figterm.get(&session).unwrap();
        assert_eq!(session.intercept, InterceptMode::Unlocked);
        assert_eq!(session.intercept_global, InterceptMode::Unlocked);
        assert!(session.sender.diagnostics().closed);
    });
}

#[gpui::test]
fn cold_tab_requests_current_rows_and_settles_without_stale_insertions(cx: &mut gpui::TestAppContext) {
    const CHILD: &str = "FASTAB_COLD_TAB_REGRESSION_CHILD";
    const TEST: &str = "overlay::tab_tests::cold_tab_requests_current_rows_and_settles_without_stale_insertions";
    if std::env::var(CHILD).as_deref() != Ok(TEST) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, TEST)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use fastab_settings::JsonStore as _;
    *fastab_settings::OldSettings::data_lock().write() = Some(
        [("autocomplete.history.disableLoading".into(), serde_json::json!(true))]
            .into_iter()
            .collect(),
    );
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[
        ("beta.history.mode", serde_json::json!("off")),
        ("autocomplete.onlyShowOnTab", serde_json::json!(true)),
    ]));
    let specs = tempfile::tempdir().unwrap();
    std::fs::write(
        specs.path().join("cold-tab-fixture.json"),
        r#"{"names":["cold-tab-fixture"],"args":[{"suggestions":[{"names":["checkout"]}]}]}"#,
    )
    .unwrap();
    let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
    let (proxy, events) = crate::event_loop::channel();
    let figterm = Arc::new(FigtermState::new());
    let platform = Arc::new(PlatformState::new(proxy.clone()));
    let session = Uuid::new_v4();
    let (sender, mut commands) = fastab_remote_ipc::outbox::channel();
    figterm.insert(FigtermSession {
        id: session,
        secret: String::new(),
        sender,
        dead_since: None,
        edit_buffer: EditBuffer::default(),
        last_receive: tokio::time::Instant::now(),
        context: None,
        flattened_env: Arc::new(Vec::new()),
        terminal_cursor_coordinates: None,
        current_session_metrics: None,
        intercept: InterceptMode::Unlocked,
        intercept_global: InterceptMode::Unlocked,
    });
    let mut controller = cx.update(|cx| {
        let mut controller =
            OverlayController::start(cx, engine.clone(), proxy, figterm.clone(), platform.clone()).unwrap();
        controller.enabled = true;
        controller
    });
    let input = |controller: &mut OverlayController, buffer: &str, cx: &mut App| {
        figterm.with(&session, |session| {
            session.edit_buffer = EditBuffer {
                text: buffer.into(),
                cursor: buffer.len() as i64,
            };
        });
        controller.complete_buffer(
            buffer.into(),
            String::new(),
            buffer.len() as u32,
            session,
            figterm.clone(),
            cx,
        );
    };
    let tab = |controller: &mut OverlayController, cx: &mut App| {
        controller.handle_action("showAutocompleteFromTab", session, &figterm, cx);
    };
    let position = WindowPosition::Absolute(Position::Logical(LogicalPosition::new(100.0, 100.0)));
    cx.update(|cx| {
        for buffer in ["cold-tab-fixture c", "cold-tab-fixture ch", "cold-tab-fixture che"] {
            input(&mut controller, buffer, cx);
            assert!(!controller.wants_caret(cx));
        }
        assert!(controller.state.read(cx).items.is_empty());
        assert_eq!(figterm.get(&session).unwrap().intercept_global, InterceptMode::Locked);
        controller.set_enabled(false, cx);
        assert_eq!(figterm.get(&session).unwrap().intercept_global, InterceptMode::Unlocked);
        controller.set_enabled(true, cx);
        assert_eq!(figterm.get(&session).unwrap().intercept_global, InterceptMode::Locked);
        tab(&mut controller, cx);
        assert!(controller.wants_caret(cx));
        assert!(insertions(&mut commands).is_empty());
    });
    assert_eq!(
        futures::executor::block_on(engine.diagnostics())
            .unwrap()
            .requests
            .submitted,
        0
    );
    cx.update(|cx| controller.apply_position(position, &platform, cx));
    futures::executor::block_on(engine.diagnostics()).unwrap();
    cx.run_until_parked();
    let (generation, result, cwd) = events
        .try_iter()
        .find_map(|event| match event {
            Event::GpuiOverlayComplete {
                generation,
                result,
                cwd,
                ..
            } => Some((generation, result.unwrap(), cwd)),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.suggestions.len(), 1);
    assert_eq!(result.suggestions[0].name, "checkout");
    cx.update(|cx| {
        controller.apply_completion(generation, Ok(result.clone()), session, &cwd, cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("ckout".into()), Some(0), Some(false))]
        );
        assert!(controller.show_intent.is_none());

        // No-caret settlement returns one Tab once, then rejects an old row.
        controller.hide_until_shown(cx);
        tab(&mut controller, cx);
        let token = controller.show_intent.as_ref().unwrap().token;
        let stale = controller.generation.load(Ordering::Relaxed);
        controller.expire_show(token, cx);
        controller.expire_show(token, cx);
        controller.apply_completion(stale, Ok(result.clone()), session, "", cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("\t".into()), Some(0), Some(false))]
        );
        assert!(!controller.wants_caret(cx));

        // An empty current result returns Tab through the same real outbox.
        tab(&mut controller, cx);
        let generation = controller.generation.load(Ordering::Relaxed);
        controller.apply_completion(generation, Ok(CompleteResult::default()), session, "", cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("\t".into()), Some(0), Some(false))]
        );

        // Double Tab during cold generation is returned as one ordered pair.
        tab(&mut controller, cx);
        tab(&mut controller, cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("\t\t".into()), Some(0), Some(false))]
        );
        assert!(controller.show_intent.is_none());

        tab(&mut controller, cx);
        controller.clear_caret_position(cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("\t".into()), Some(0), Some(false))]
        );

        // Show is display-only. Dangerous rows and unfinished generators are
        // never eligible for the private hidden-Tab fast acceptance path.
        controller.show(cx);
        assert!(!controller.accept_single_show_result(&result));
        controller.hide_until_shown(cx);
        tab(&mut controller, cx);
        for kind in ["auto-execute", "special"] {
            let mut unsafe_result = result.clone();
            unsafe_result.suggestions[0].kind = kind.into();
            assert!(!controller.accept_single_show_result(&unsafe_result));
        }
        for insertion in ["\n", "checkout\r"] {
            let mut unsafe_result = result.clone();
            unsafe_result.suggestions[0].insert_value = Some(insertion.into());
            assert!(!controller.accept_single_show_result(&unsafe_result));
        }
        let mut dangerous = result.clone();
        dangerous.suggestions[0].is_dangerous = true;
        assert!(!controller.accept_single_show_result(&dangerous));
        let mut multiple = result.clone();
        multiple.suggestions.push(multiple.suggestions[0].clone());
        assert!(!controller.accept_single_show_result(&multiple));
        let mut pending = result.clone();
        pending.pending_generators = true;
        assert!(!controller.accept_single_show_result(&pending));
        assert!(insertions(&mut commands).is_empty());

        // Model the visible loading state without opening an NSWindow (the
        // GPUI test platform has no raw native window handle). Actual showing
        // cancels icon idle retirement and owns the loading marker this way.
        let token = controller.show_intent.as_ref().unwrap().token;
        let stale = controller.generation.load(Ordering::Relaxed);
        controller.file_icons.update(cx, |icons, _| icons.cancel_idle());
        controller.loading_owner.store(stale, Ordering::Relaxed);
        controller.state.update(cx, |state, _| {
            state.set_suggestions(vec![row("checkout", "arg")], "che".into());
            state.loading = true;
        });
        // Exactly one added character avoids the separate paste/backspace
        // hide path, so this exercises explicit-request cancellation itself.
        input(&mut controller, "cold-tab-fixture chec", cx);
        controller.expire_show(token, cx);
        controller.apply_completion(stale, Ok(result.clone()), session, "", cx);
        assert!(insertions(&mut commands).is_empty());
        assert_eq!(controller.loading_owner.load(Ordering::Relaxed), NO_LOADING_OWNER);
        assert!(controller.show_intent.is_none());
        let state = controller.state.read(cx);
        assert!(!state.visible && !state.loading && state.suppress_until_shown);
        assert_eq!(state.items.len(), 1, "the hide starts the normal grace period");
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(9));
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(controller.state.read(cx).items.len(), 1));
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        events
            .try_iter()
            .all(|event| !matches!(event, Event::GpuiOverlayShowExpired { .. })),
        "settled Show intents must cancel their timer instead of enqueueing stale expirations"
    );
    cx.update(|cx| {
        assert_eq!(
            controller.state.read(cx).items.capacity(),
            0,
            "superseding a visible loading request must arm idle payload retirement"
        );
        // Switching terminals also cancels ownership without a stale replay.
        controller.hide_until_shown(cx);
        tab(&mut controller, cx);
        let token = controller.show_intent.as_ref().unwrap().token;
        controller.complete_buffer(
            "other input".into(),
            String::new(),
            11,
            Uuid::new_v4(),
            figterm.clone(),
            cx,
        );
        controller.expire_show(token, cx);
        assert!(insertions(&mut commands).is_empty());
    });
}

fn show_outbox_fixture(
    cx: &mut gpui::TestAppContext,
    buffer: &str,
) -> (
    OverlayController,
    Arc<FigtermState>,
    fastab_remote_ipc::outbox::Outbox,
    Uuid,
) {
    let specs = tempfile::tempdir().unwrap();
    let engine = EngineClient::spawn(specs.path().to_path_buf()).unwrap();
    let (proxy, _events) = crate::event_loop::channel();
    let figterm = Arc::new(FigtermState::new());
    let platform = Arc::new(PlatformState::new(proxy.clone()));
    let session = Uuid::new_v4();
    let (sender, commands) = fastab_remote_ipc::outbox::channel();
    figterm.insert(FigtermSession {
        id: session,
        secret: String::new(),
        sender,
        dead_since: None,
        edit_buffer: EditBuffer {
            text: buffer.into(),
            cursor: buffer.len() as i64,
        },
        last_receive: tokio::time::Instant::now(),
        context: None,
        flattened_env: Arc::new(Vec::new()),
        terminal_cursor_coordinates: None,
        current_session_metrics: None,
        intercept: InterceptMode::Unlocked,
        intercept_global: InterceptMode::Unlocked,
    });
    let controller = cx.update(|cx| {
        cx.set_global(HeadlessOverlayWindows);
        let mut controller = OverlayController::start(cx, engine, proxy, figterm.clone(), platform).unwrap();
        controller.enabled = true;
        // onlyShowOnTab and an absent caret keep the actual input entry point
        // from running a worker completion. Results below enter explicitly.
        controller.complete_buffer(
            buffer.into(),
            String::new(),
            buffer.len() as u32,
            session,
            figterm.clone(),
            cx,
        );
        controller
    });
    (controller, figterm, commands, session)
}

fn partial_show_result(search: &str, arg: &str) -> CompleteResult {
    CompleteResult {
        suggestions: vec![fastab_engine::Suggestion {
            name: "checkout".into(),
            kind: "arg".into(),
            argument_value: true,
            ..Default::default()
        }],
        search_term: search.into(),
        current_arg: Some(fastab_engine::CurrentArg {
            name: arg.into(),
            ..Default::default()
        }),
        pending_generators: true,
        ..Default::default()
    }
}

#[gpui::test]
fn hidden_tab_discards_old_rows_before_loading_or_acceptance(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[(
        "autocomplete.onlyShowOnTab",
        serde_json::json!(true),
    )]));
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "show-fixture ch");
    cx.update(|cx| {
        controller.state.update(cx, |state, _| {
            state.suppress_until_shown = false;
            state.set_suggestions(
                vec![SuggestionItem {
                    argument_value: true,
                    ..row("checkout", "arg")
                }],
                "ch".into(),
            );
        });
        controller.hide_until_shown(cx);
        let buffer = "show-fixture chzz";
        figterm.with(&session, |session| {
            session.edit_buffer = EditBuffer {
                text: buffer.into(),
                cursor: buffer.len() as i64,
            };
        });
        controller.complete_buffer(
            buffer.into(),
            String::new(),
            buffer.len() as u32,
            session,
            figterm.clone(),
            cx,
        );
        assert_eq!(
            controller.state.read(cx).items.len(),
            1,
            "hidden rows survive their grace period"
        );
        controller.handle_action("showAutocompleteFromTab", session, &figterm, cx);
        assert!(
            controller.state.read(cx).items.is_empty(),
            "the new Tab cannot expose old edits"
        );
        let generation = controller.generation.load(Ordering::Relaxed);
        controller.show_loading(generation, cx);
        assert!(controller.state.read(cx).loading);
        controller.handle_action("insertSelected", session, &figterm, cx);
        controller.handle_click(
            ClickInsert {
                name: "checkout".into(),
                search: "ch".into(),
                kind: "arg".into(),
                argument_value: true,
                ..Default::default()
            },
            session,
            generation,
            &figterm,
            cx,
        );
        assert!(
            insertions(&mut commands).is_empty(),
            "loading must reject Enter and the old click"
        );
        assert_eq!(controller.show_intent.as_ref().unwrap().tabs, 1);
    });
}

#[gpui::test]
fn actionable_partial_and_manual_acceptance_consume_the_original_tab(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[(
        "autocomplete.onlyShowOnTab",
        serde_json::json!(true),
    )]));
    for (action, positioned) in [
        ("insertSelected", true),
        ("click", true),
        ("insertCommonPrefixOrInsertSelected", true),
        ("hideAutocomplete", true),
        ("insertSelected", false),
    ] {
        let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "show-fixture ch");
        cx.update(|cx| {
            controller.handle_action("showAutocompleteFromTab", session, &figterm, cx);
            let token = controller.show_intent.as_ref().unwrap().token;
            let generation = controller.generation.load(Ordering::Relaxed);
            controller.apply_completion(generation, Ok(partial_show_result("ch", "branch")), session, "", cx);
            assert!(controller.show_intent.as_ref().unwrap().result_ready);
            assert!(controller.explicit_show_pending);
            if positioned {
                // The headless GPUI platform cannot position an NSWindow.
                // Invoke the production acknowledgement with its success.
                controller.settle_show_layout(true);
                assert!(controller.show_intent.is_none());
            }
            if action == "click" {
                controller.handle_click(
                    ClickInsert {
                        name: "checkout".into(),
                        search: "ch".into(),
                        kind: "arg".into(),
                        argument_value: true,
                        ..Default::default()
                    },
                    session,
                    generation,
                    &figterm,
                    cx,
                );
            } else {
                controller.handle_action(action, session, &figterm, cx);
            }
            controller.expire_show(token, cx);
            controller.hide_until_shown(cx);
            let expected = if action == "hideAutocomplete" {
                vec![]
            } else {
                vec![(Some("eckout".into()), Some(0), Some(false))]
            };
            assert_eq!(insertions(&mut commands), expected, "{action}, positioned={positioned}");
            assert!(controller.show_intent.is_none());
            assert!(!controller.explicit_show_pending);
        });
    }
}

#[gpui::test]
fn empty_pending_result_keeps_tab_debt_until_expiry(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[(
        "autocomplete.onlyShowOnTab",
        serde_json::json!(true),
    )]));
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "show-fixture ch");
    cx.update(|cx| {
        controller.handle_action("showAutocompleteFromTab", session, &figterm, cx);
        let token = controller.show_intent.as_ref().unwrap().token;
        let generation = controller.generation.load(Ordering::Relaxed);
        controller.apply_completion(generation, Ok(partial_show_result("ch", "branch")), session, "", cx);
        assert!(controller.show_intent.as_ref().unwrap().result_ready);
        controller.apply_completion(
            generation,
            Ok(CompleteResult {
                pending_generators: true,
                ..Default::default()
            }),
            session,
            "",
            cx,
        );
        assert!(!controller.show_intent.as_ref().unwrap().result_ready);
        controller.settle_show_layout(true);
        assert!(
            controller.show_intent.is_some(),
            "empty loading cannot consume the intercepted Tab"
        );
        assert!(insertions(&mut commands).is_empty());
        controller.expire_show(token, cx);
        controller.expire_show(token, cx);
        assert_eq!(
            insertions(&mut commands),
            vec![(Some("\t".into()), Some(0), Some(false))]
        );
        assert!(!controller.explicit_show_pending);
    });
}

#[gpui::test]
fn explicit_partial_show_survives_argument_change_and_retires_at_boundaries(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[(
        "autocomplete.onlyShowOnTab",
        serde_json::json!(true),
    )]));
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "show-fixture ch");
    cx.update(|cx| {
        controller.handle_action("showAutocomplete", session, &figterm, cx);
        let generation = controller.generation.load(Ordering::Relaxed);
        controller.apply_completion(generation, Ok(partial_show_result("ch", "first")), session, "", cx);
        controller.settle_show_layout(true);
        assert!(controller.show_intent.is_none());
        assert!(controller.explicit_show_pending);
        let mut final_result = partial_show_result("ch", "second");
        final_result.pending_generators = false;
        controller.apply_completion(generation, Ok(final_result), session, "", cx);
        let state = controller.state.read(cx);
        assert_eq!(state.current_arg_name, "second");
        assert!(
            state.visible && !state.suppress_until_shown,
            "the final explicit result must remain visible"
        );
        assert!(!controller.explicit_show_pending);

        for hide in [false, true] {
            controller.hide_until_shown(cx);
            controller.handle_action("showAutocomplete", session, &figterm, cx);
            let generation = controller.generation.load(Ordering::Relaxed);
            controller.apply_completion(generation, Ok(partial_show_result("ch", "branch")), session, "", cx);
            controller.settle_show_layout(true);
            assert!(controller.explicit_show_pending);
            if hide {
                controller.hide_until_shown(cx);
            } else {
                let buffer = "show-fixture cha";
                figterm.with(&session, |session| {
                    session.edit_buffer = EditBuffer {
                        text: buffer.into(),
                        cursor: buffer.len() as i64,
                    };
                });
                controller.complete_buffer(
                    buffer.into(),
                    String::new(),
                    buffer.len() as u32,
                    session,
                    figterm.clone(),
                    cx,
                );
            }
            assert!(!controller.explicit_show_pending);
        }
        assert!(insertions(&mut commands).is_empty(), "explicit Show is display-only");
    });
}

#[gpui::test]
fn insert_on_rows_for_an_older_input_waits_for_the_current_rows(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "git ch");
    let input = |buffer: &str| LastInput {
        buffer: buffer.into(),
        cwd: String::new(),
        cursor: buffer.len() as u32,
        session_id: session,
    };
    let result = |search: &str| CompleteResult {
        suggestions: vec![fastab_engine::Suggestion {
            name: "checkout".into(),
            kind: "subcommand".into(),
            ..Default::default()
        }],
        search_term: search.into(),
        ..Default::default()
    };
    cx.update(|cx| {
        for next in ["git chec", "git che"] {
            // Rows for `ch` stay on screen while `git che` is in flight.
            controller.state.update(cx, |state, _| {
                state.suppress_until_shown = false;
                state.set_suggestions(vec![row("checkout", "subcommand")], "ch".into());
            });
            controller.rows_input = Some(input("git ch"));
            *controller.last_input.lock().unwrap() = Some(input("git che"));
            controller.handle_action("insertSelected", session, &figterm, cx);
            assert!(insertions(&mut commands).is_empty(), "`eckout` would garble `che`");
            assert_eq!(controller.pending_insert_action.as_ref().unwrap().1, input("git che"));
            let generation = controller.generation.load(Ordering::Relaxed);
            controller.request_input = Some((generation, input(next)));
            if next == "git chec" {
                // Typing on retires the action: it never lands on newer rows.
                controller.apply_completion(generation, Ok(result("chec")), session, "", cx);
                assert!(controller.pending_insert_action.is_none());
                assert!(insertions(&mut commands).is_empty());
            } else {
                controller.apply_completion(generation, Ok(result("che")), session, "", cx);
                assert!(controller.pending_insert_action.is_none());
                assert_eq!(
                    insertions(&mut commands),
                    vec![(Some("ckout".into()), Some(0), Some(false))]
                );
            }
        }
    });
}

#[gpui::test]
fn a_deferred_insert_does_not_outlive_a_cancelled_list(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "git ch");
    let input = |buffer: &str| LastInput {
        buffer: buffer.into(),
        cwd: String::new(),
        cursor: buffer.len() as u32,
        session_id: session,
    };
    let cancels: [fn(&mut OverlayController, &mut App); 3] = [
        OverlayController::hide_until_shown,
        OverlayController::dismiss,
        OverlayController::hide,
    ];
    cx.update(|cx| {
        for cancel in cancels {
            let show_rows = |controller: &mut OverlayController, cx: &mut App| {
                controller.state.update(cx, |state, _| {
                    state.suppress_until_shown = false;
                    state.set_suggestions(vec![row("checkout", "subcommand")], "ch".into());
                });
            };
            show_rows(&mut controller, cx);
            controller.rows_input = Some(input("git ch"));
            *controller.last_input.lock().unwrap() = Some(input("git che"));
            controller.handle_action("insertSelected", session, &figterm, cx);
            assert!(controller.pending_insert_action.is_some());
            cancel(&mut controller, cx);
            assert!(controller.pending_insert_action.is_none());
            // A later Show for the same input must only show the list.
            show_rows(&mut controller, cx);
            let generation = controller.generation.load(Ordering::Relaxed);
            controller.request_input = Some((generation, input("git che")));
            let result = CompleteResult {
                suggestions: vec![fastab_engine::Suggestion {
                    name: "checkout".into(),
                    kind: "subcommand".into(),
                    ..Default::default()
                }],
                search_term: "che".into(),
                ..Default::default()
            };
            controller.apply_completion(generation, Ok(result), session, "", cx);
            assert!(insertions(&mut commands).is_empty());
        }
    });
}

#[gpui::test]
fn a_deferred_tab_with_nothing_to_insert_reaches_the_shell(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let (mut controller, figterm, mut commands, session) = show_outbox_fixture(cx, "ls -l");
    let input = |buffer: &str| LastInput {
        buffer: buffer.into(),
        cwd: String::new(),
        cursor: buffer.len() as u32,
        session_id: session,
    };
    let set_line = |buffer: &str| {
        figterm.with(&session, |session| {
            session.edit_buffer.update(buffer, buffer.len() as i64)
        });
    };
    cx.update(|cx| {
        // An empty result, a failed one, and an empty result after the line
        // moved on. A late Enter could run a newer line, so it never returns.
        for (key, failed, line_after, returned) in [
            ("\t", false, "ls -la", true),
            ("\t", true, "ls -la", true),
            ("\t", false, "ls -la ", false),
            ("\r", false, "ls -la", false),
        ] {
            controller.state.update(cx, |state, _| {
                state.suppress_until_shown = false;
                state.set_suggestions(vec![row("-l", "option")], "-l".into());
            });
            controller.rows_input = Some(input("ls -l"));
            *controller.last_input.lock().unwrap() = Some(input("ls -la"));
            set_line("ls -la");
            controller.handle_key_action("insertSelected", Some(key.into()), session, &figterm, cx);
            assert!(insertions(&mut commands).is_empty());
            set_line(line_after);
            let generation = controller.generation.load(Ordering::Relaxed);
            controller.request_input = Some((generation, input("ls -la")));
            let result = if failed {
                Err(anyhow::anyhow!("engine failed"))
            } else {
                Ok(CompleteResult {
                    search_term: "-la".into(),
                    ..Default::default()
                })
            };
            controller.apply_completion(generation, result, session, "", cx);
            assert!(controller.pending_insert_action.is_none());
            let expected = if returned {
                vec![(Some(key.into()), Some(0), Some(false))]
            } else {
                Vec::new()
            };
            assert_eq!(
                insertions(&mut commands),
                expected,
                "key={key:?} failed={failed} line={line_after:?}"
            );
        }
    });
}

#[gpui::test]
fn a_background_prompt_does_not_take_over_the_typing_session(cx: &mut gpui::TestAppContext) {
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::new_fake());
    let (mut controller, figterm, _commands, session) = show_outbox_fixture(cx, "git ch");
    let background = Uuid::new_v4();
    cx.update(|cx| {
        controller.state.update(cx, |state, _| {
            state.suppress_until_shown = false;
            state.set_suggestions(vec![row("checkout", "subcommand")], "ch".into());
        });
        set_intercept(&figterm, session, true, true);
        for prompt in ["", "  "] {
            controller.complete_buffer(prompt.into(), String::new(), 0, background, figterm.clone(), cx);
            assert_eq!(controller.current_session(), Some(session));
            assert_eq!(figterm.get(&session).unwrap().intercept, InterceptMode::Locked);
            assert_eq!(controller.last_input.lock().unwrap().as_ref().unwrap().buffer, "git ch");
            let state = controller.state.read(cx);
            assert!(state.visible);
            assert_eq!(state.items.len(), 1);
        }
    });
}
