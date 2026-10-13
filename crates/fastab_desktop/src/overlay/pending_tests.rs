use super::*;

#[gpui::test]
fn invisible_pending_generator_finishes_in_normal_mode(cx: &mut gpui::TestAppContext) {
    pending_generator_finishes(
        cx,
        false,
        "overlay::pending_tests::invisible_pending_generator_finishes_in_normal_mode",
    );
}

#[gpui::test]
fn explicit_show_runs_pending_generator_in_tab_only_mode(cx: &mut gpui::TestAppContext) {
    pending_generator_finishes(
        cx,
        true,
        "overlay::pending_tests::explicit_show_runs_pending_generator_in_tab_only_mode",
    );
}

fn pending_generator_finishes(cx: &mut gpui::TestAppContext, only_show_on_tab: bool, test_name: &str) {
    // EngineClient runs on real threads, outside the thread-local UI settings
    // override. Isolate process-wide history/script settings from the user's
    // configuration and from concurrently running tests.
    const CHILD: &str = "FASTAB_PENDING_GENERATOR_REGRESSION_CHILD";
    if std::env::var(CHILD).as_deref() != Ok(test_name) {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, test_name)
            .output()
            .expect("isolated pending-generator regression process");
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
        [
            ("autocomplete.history.disableLoading".into(), serde_json::json!(true)),
            ("autocomplete.sortMethod".into(), serde_json::json!("default")),
            ("autocomplete.scriptTimeout".into(), serde_json::json!(2000)),
        ]
        .into_iter()
        .collect(),
    );
    let _settings = fastab_settings::settings::install_override(fastab_settings::Settings::from_slice(&[
        ("dashboard.theme", serde_json::json!("light")),
        ("beta.history.mode", serde_json::json!("off")),
        ("autocomplete.onlyShowOnTab", serde_json::json!(only_show_on_tab)),
    ]));
    let specs = tempfile::tempdir().expect("spec fixture directory");
    let cwd = tempfile::tempdir().expect("generator working directory");
    let marker = cwd.path().join("invocations");
    const DEBOUNCE_MS: u64 = 50;
    // No argument name/description: those would keep a description-only
    // overlay visible and miss the initially empty, invisible result.
    std::fs::write(
        specs.path().join("fastab-pending-fixture.json"),
        serde_json::to_vec(&serde_json::json!({
            "names": ["fastab-pending-fixture"],
            "args": [{
                "script": [
                    "/bin/sh", "-c",
                    "printf 'run\\n' >> invocations; printf 'dynamic-result\\n'"
                ],
                "split_on": "\n",
                "debounce_ms": DEBOUNCE_MS
            }]
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture spec");
    let engine = EngineClient::spawn(specs.path().to_path_buf()).expect("engine");
    let (proxy, events) = crate::event_loop::channel();
    let figterm_state = Arc::new(FigtermState::new());
    let platform_state = Arc::new(PlatformState::new(proxy.clone()));
    let mut overlay = cx.update(|cx| {
        let mut overlay =
            OverlayController::start(cx, engine.clone(), proxy, figterm_state.clone(), platform_state.clone())
                .expect("controller");
        // Headless GPUI coverage does not depend on the test binary's AX grant.
        overlay.enabled = true;
        overlay
    });
    let session = Uuid::new_v4();
    let buffer = "fastab-pending-fixture ";
    cx.update(|cx| {
        overlay.complete_buffer(
            buffer.into(),
            cwd.path().to_str().expect("fixture path").into(),
            buffer.len() as u32,
            session,
            figterm_state,
            cx,
        );
        if only_show_on_tab {
            assert!(!overlay.wants_caret(cx));
            overlay.show(cx);
        }
        overlay.apply_position(
            WindowPosition::Absolute(Position::Logical(LogicalPosition::new(100.0, 100.0))),
            &platform_state,
            cx,
        );
    });

    let (generation, first, first_session, first_cwd) = completion_event(cx, &engine, &events);
    assert_eq!(first_session, session);
    assert!(first.pending_generators, "the real engine must defer the script");
    assert!(first.suggestions.is_empty(), "the first pass must have no static rows");
    assert!(!marker.exists(), "the deferred script must not have run yet");
    cx.update(|cx| {
        overlay.apply_completion(generation, Ok(first), first_session, &first_cwd, cx);
        let state = overlay.state.read(cx);
        assert!(state.items.is_empty());
        assert!(state.current_arg_name.is_empty() && state.current_arg_description.is_empty());
        assert!(!state.visible, "the pending result must exercise the invisible branch");
    });

    // Run the actual GPUI timer, then dispatch its real host event. Do not
    // synthesize a debounce event: its absence is the regression being tested.
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(DEBOUNCE_MS));
    cx.run_until_parked();
    let (debounce_generation, waited_ms) = events
        .try_iter()
        .find_map(|event| match event {
            Event::GpuiOverlayDebouncedComplete { generation, waited_ms } => Some((generation, waited_ms)),
            _ => None,
        })
        .expect("an invisible pending result must still schedule its debounce event");
    assert_eq!(debounce_generation, generation);
    assert_eq!(waited_ms, DEBOUNCE_MS);
    cx.update(|cx| overlay.apply_debounced_complete(debounce_generation, waited_ms, cx));

    let (generation, second, second_session, second_cwd) = completion_event(cx, &engine, &events);
    assert_eq!(second_session, session);
    assert!(!second.pending_generators, "the follow-up must complete the generator");
    assert_eq!(
        second
            .suggestions
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["dynamic-result"]
    );
    assert_eq!(std::fs::read_to_string(marker).expect("script invocation"), "run\n");
    // Visible results need a native NSWindow, unavailable in TestAppContext.
    // Both modes have now exercised the actual worker and debounce timer.
    cx.update(|cx| overlay.end_input(session, cx));
    assert_eq!(
        futures::executor::block_on(engine.diagnostics())
            .expect("final diagnostics")
            .requests
            .completed,
        2,
        "one initial pass and one real debounce follow-up"
    );
}

fn completion_event(
    cx: &mut gpui::TestAppContext,
    engine: &EngineClient,
    events: &crate::event_loop::EventLoopReceiver,
) -> (u64, CompleteResult, Uuid, String) {
    // This FIFO barrier waits for the real worker before GPUI polls its
    // completion future, avoiding wall-clock races with the loading timer.
    futures::executor::block_on(engine.diagnostics()).expect("completion barrier");
    cx.run_until_parked();
    events
        .try_iter()
        .find_map(|event| match event {
            Event::GpuiOverlayComplete {
                generation,
                result,
                session_id,
                cwd,
            } => Some((generation, result.expect("real engine completion"), session_id, cwd)),
            _ => None,
        })
        .expect("the real completion task must emit its host event")
}
