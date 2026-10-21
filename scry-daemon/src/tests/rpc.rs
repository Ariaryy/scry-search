use super::*;

#[test]
fn an_old_pending_observation_cannot_cancel_the_request_that_consumed_it() {
    let state = Arc::new(ConnectionCancellation::default());
    let (observed, seen) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let observer_state = state.clone();
    let observer = std::thread::spawn(move || {
        observer_state
            .observe_pending(|| {
                // Simulate a peek of the current request, delayed before the
                // generation increment while the server consumes its frame.
                observed.send(()).unwrap();
                released.recv().unwrap();
                Ok(1)
            })
            .unwrap();
    });
    seen.recv().unwrap();
    assert!(state.observation.try_lock().is_err());
    let (started, start) = std::sync::mpsc::channel();
    let request_state = state.clone();
    let request = std::thread::spawn(move || {
        let cancel = request_state.begin_request();
        started.send(cancel.is_cancelled()).unwrap();
    });
    assert!(start
        .recv_timeout(std::time::Duration::from_millis(20))
        .is_err());
    release.send(()).unwrap();
    observer.join().unwrap();
    assert!(!start.recv().unwrap());
    request.join().unwrap();
    let cancel = state.begin_request();
    state.observe_pending(|| Ok(0)).unwrap();
    assert!(!cancel.is_cancelled());
    state.observe_pending(|| Ok(1)).unwrap();
    assert!(
        cancel.is_cancelled(),
        "a genuinely newer request must still cancel"
    );
}

#[test]
fn local_and_rpc_results_agree() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_store_with_n_records(200, &dir);
    let expected = view.search(&Query::Substring("file".into()), 50);
    let indexes = Arc::new(vec![VolumeIndex {
        volume: "C:".to_string(),
        store: Arc::new(arc_swap::ArcSwap::from(view)),
        cursor: None,
    }]);
    let pipe_name = format!(
        r"\\.\pipe\scry-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let server_indexes = indexes.clone();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        handle_connection(pipe, &server_indexes).unwrap();
    });
    let mut client = (0..100)
        .find_map(|_| {
            scry_client::Client::connect_to(&pipe_name)
                .ok()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                })
        })
        .expect("test pipe did not become ready");

    // Same query, both paths, over the same live connection: the RPC
    // round trip and the in-process local mapping must agree exactly.
    let rpc = client.query(QueryKind::Substring, "file", 50).unwrap();
    let local = client
        .search_local(QueryKind::Substring, "file", 50)
        .unwrap();
    assert_eq!(rpc, expected);
    assert_eq!(rpc, local);
    assert_eq!(
        client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap(),
        expected
    );

    drop(client);
    thread.join().unwrap();
}

#[test]
fn shared_section_key_is_unique_per_generation() {
    // Two views sharing the exact same `base` allocation (as happens right
    // before the allocator would reuse a freed generation's address) must
    // still get distinct cache keys, or `shared_section` hands out a stale
    // mapping under a key that looks fresh (the ABA hazard this guards).
    let dir = tempfile::tempdir().unwrap();
    let view1 = build_store_with_n_records(10, &dir);
    let view2 = IndexView {
        base: view1.base.clone(),
        delta: view1.delta.clone(),
        generation: view1.generation + 1,
        journal_id: view1.journal_id,
        next_usn: view1.next_usn,
        volume_serial: view1.volume_serial,
    };
    assert_ne!(
        shared_section_cache_key(&view1),
        shared_section_cache_key(&view2),
        "same base allocation, different generation: keys must not collide"
    );
}

#[test]
fn shared_section_cache_keeps_base_alive() {
    let dir = tempfile::tempdir().unwrap();
    let cache: SectionCache = std::sync::Mutex::new(Vec::new());
    let view = build_store_with_n_records(10, &dir);
    let base_weak = Arc::downgrade(&view.base);
    shared_section_from_cache(&view, &cache).unwrap();
    // Drop every strong reference this test holds directly; the cache
    // entry inserted by `shared_section_from_cache` must be the one
    // keeping it alive.
    drop(view);
    assert!(
        base_weak.upgrade().is_some(),
        "cached section entry did not keep its base alive"
    );
}

#[test]
fn shared_section_cache_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let cache: SectionCache = std::sync::Mutex::new(Vec::new());
    let stale = build_store_with_n_records(10, &dir);
    let stale_weak = Arc::downgrade(&stale.base);
    shared_section_from_cache(&stale, &cache).unwrap();
    drop(stale);
    for n in 11..11 + SHARED_SECTION_CACHE_GENERATIONS {
        let view = build_store_with_n_records(n, &dir);
        shared_section_from_cache(&view, &cache).unwrap();
    }
    assert!(
        stale_weak.upgrade().is_none(),
        "cache grew past its documented bound of {SHARED_SECTION_CACHE_GENERATIONS} generations"
    );
    assert!(cache.lock().unwrap().len() <= SHARED_SECTION_CACHE_GENERATIONS);
}

#[test]
fn cancelled_request_returns_empty_results() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_store_with_n_records(200, &dir);
    let indexes = Arc::new(vec![VolumeIndex {
        volume: "C:".to_string(),
        store: Arc::new(arc_swap::ArcSwap::from(view)),
        cursor: None,
    }]);
    let generation = std::sync::atomic::AtomicU64::new(1);
    let cancel = scry_core::Cancellation::new(&generation, 0); // stale: expects 0, generation is 1
    let entries = search_indexes_cancellable(
        &indexes,
        &Query::Substring("file".into()),
        SearchOptions::new(50),
        cancel,
    );
    assert!(entries.is_empty());
}

#[test]
fn two_pipelined_requests_produce_exactly_two_response_frames() {
    let dir = tempfile::tempdir().unwrap();
    let view = build_store_with_n_records(200, &dir);
    let expected = view.search(&Query::Substring("file".into()), 50);
    let indexes = Arc::new(vec![VolumeIndex {
        volume: "C:".to_string(),
        store: Arc::new(arc_swap::ArcSwap::from(view)),
        cursor: None,
    }]);
    let pipe_name = format!(
        r"\\.\pipe\scry-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let server_indexes = indexes.clone();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        handle_connection(pipe, &server_indexes).unwrap();
    });
    let client = (0..100)
        .find_map(|_| {
            scry_ipc::connect_client(&pipe_name).ok().or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                None
            })
        })
        .expect("test pipe did not become ready");

    let request = scry_core::protocol::Request {
        kind: QueryKind::Substring,
        pattern: "file".to_string(),
        limit: 50,
        order: Order::default(),
    };
    // Both requests are written before either response is read, so the
    // daemon's peek thread should see the second one arrive while (or
    // before) the first is still being handled — but whether it actually
    // supersedes the first is a timing race, not something this test can
    // assert on. What must always hold is the framing invariant: exactly
    // one response frame per request, in request order.
    client
        .write_frame(&scry_core::protocol::encode_request(&request))
        .unwrap();
    client
        .write_frame(&scry_core::protocol::encode_request(&request))
        .unwrap();

    let first = scry_core::protocol::decode_results(&client.read_frame().unwrap()).unwrap();
    let second = scry_core::protocol::decode_results(&client.read_frame().unwrap()).unwrap();
    assert!(first.is_empty() || first == expected);
    assert_eq!(second, expected);

    drop(client);
    thread.join().unwrap();
}

/// The console handler exits the process, so only the classification is
/// tested directly. `CTRL_C`/`BREAK`/`CLOSE`/`LOGOFF`/`SHUTDOWN` are the
/// signals that mean the process is going away; anything else (Windows
/// reserves the range for future use) must not trigger a shutdown write.
#[test]
fn only_termination_signals_are_treated_as_shutdown() {
    assert!(is_shutdown_signal(ffi::CTRL_C_EVENT));
    assert!(is_shutdown_signal(ffi::CTRL_BREAK_EVENT));
    assert!(is_shutdown_signal(ffi::CTRL_CLOSE_EVENT));
    assert!(is_shutdown_signal(ffi::CTRL_LOGOFF_EVENT));
    assert!(is_shutdown_signal(ffi::CTRL_SHUTDOWN_EVENT));
    assert!(!is_shutdown_signal(3));
    assert!(!is_shutdown_signal(4));
}

/// Struct sizes must match their Win32 counterparts exactly — a mismatch
/// causes `SetProcessInformation` to fail with ERROR_INVALID_PARAMETER.
#[test]
fn power_throttling_state_is_24_bytes_or_less() {
    assert_eq!(std::mem::size_of::<ffi::ProcessPowerThrottlingState>(), 12);
    assert_eq!(std::mem::size_of::<ffi::MemoryPriorityInformation>(), 4);
}
