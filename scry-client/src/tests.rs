use super::local::LOCAL_HANDSHAKE_STALENESS_WINDOW;
use super::*;
use scry_core::protocol::{
    decode_request, encode_results, encode_shared_index, SharedIndexResponse,
};

#[test]
fn discovery_is_bounded_and_does_not_mistake_legacy_or_corruption_for_status() {
    for response in [
        Some(scry_core::protocol::encode_status(&DaemonStatus::current(
            true,
        ))),
        Some(b"SCRYSTA2".to_vec()),
        Some(b"SCRYUNS1".to_vec()),
        Some(vec![0; 86]),
        None,
    ] {
        let name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&name).unwrap();
        let expected = response
            .as_ref()
            .is_some_and(|b| b.starts_with(b"SCRYSTA1"));
        let legacy = response.is_none();
        let worker = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
            assert_eq!(request.kind, QueryKind::Status);
            assert_eq!(request.pattern, "SCRYSTA1");
            if let Some(bytes) = response {
                pipe.write_frame(&bytes).unwrap();
            }
            // The deployed legacy daemon consumes and ignores an unknown kind.
            std::thread::sleep(std::time::Duration::from_millis(1000));
        });
        let result = (0..100)
            .find_map(|_| {
                let result = Client::daemon_status_at(&name, std::time::Duration::from_millis(500));
                if result
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<std::io::Error>())
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                } else {
                    Some(result)
                }
            })
            .expect("test pipe did not become ready");
        assert_eq!(result.is_ok(), expected, "{result:?}");
        if legacy {
            let error = result.unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::TimedOut
            );
        }
        worker.join().unwrap();
    }
}

fn unique_pipe_name() -> String {
    format!(
        r"\\.\pipe\scry-client-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn connect(pipe_name: &str) -> Client {
    (0..100)
        .find_map(|_| {
            Client::connect_to(pipe_name).ok().or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                None
            })
        })
        .expect("test pipe did not become ready")
}

fn entry(name: &str) -> ResultEntry {
    ResultEntry {
        path: name.to_string(),
        size: 0,
        mtime: 0,
        is_dir: false,
        size_exact: true,
    }
}

/// A response already sitting in the pipe when `search_interactive` is
/// called — the answer to a keystroke the caller has since moved past —
/// must be discarded in favor of the response to the request this call
/// itself just wrote, never returned to the caller.
#[test]
fn search_interactive_discards_a_stale_buffered_response() {
    let pipe_name = unique_pipe_name();
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        // Respond to both requests only after both have arrived, so the
        // client's write of the second request races ahead of it reading
        // the first response — exactly the scenario this method exists for.
        let _first = pipe.read_frame().unwrap();
        let _second = pipe.read_frame().unwrap();
        pipe.write_frame(&encode_results(&[entry("stale")]))
            .unwrap();
        pipe.write_frame(&encode_results(&[entry("fresh")]))
            .unwrap();
    });

    let mut client = connect(&pipe_name);
    // Simulate an earlier `search_interactive` call whose request was
    // written but whose response hasn't been read yet.
    client
        .pipe
        .write_frame(&encode_request(&Request {
            kind: QueryKind::Substring,
            pattern: "stale-query".to_string(),
            limit: 50,
            order: Order::default(),
        }))
        .unwrap();
    client.pending_interactive = 1;

    let result = client
        .search_interactive(QueryKind::Substring, "fresh-query", 50)
        .unwrap();
    assert_eq!(result, vec![entry("fresh")]);

    drop(client);
    thread.join().unwrap();
}

#[test]
fn interactive_poll_never_waits_for_an_inflight_query() {
    let pipe_name = unique_pipe_name();
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        let _request = pipe.read_frame().unwrap();
        released.recv().unwrap();
        pipe.write_frame(&encode_results(&[entry("ready")]))
            .unwrap();
    });

    let mut session = connect(&pipe_name).into_search_session(50);
    session.submit(QueryKind::Substring, "query").unwrap();
    assert!(session.poll_latest().unwrap().is_none());
    assert!(session.is_pending());
    release.send(()).unwrap();

    let result = (0..100)
        .find_map(|_| {
            let result = session.poll_latest().unwrap();
            if result.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            result
        })
        .expect("response did not become readable");
    assert_eq!(result, vec![entry("ready")]);
    assert!(!session.is_pending());
    thread.join().unwrap();
}

/// Builds an in-memory, self-contained rkyv-archived arena and a real
/// section mapping it, duplicated to this process — exactly what a real
/// daemon hands back in a `ShareIndex` response, minus the file on disk.
fn shared_index_response(generation: u64) -> SharedIndexResponse {
    let mut b = scry_core::Arena::builder();
    let root = b.push("C:", 0, true);
    let child = b.push("file.txt", 0, false);
    b.set_parent(child, root);
    let arena = b.build().0;
    let bytes = scry_core::store::to_bytes(&arena).unwrap();
    let section = scry_ipc::Section::create(&bytes).unwrap();
    // The duplicated handle is independent of `section`'s own handle
    // (closed when `section` drops at the end of this scope), exactly
    // as it would be once handed to a separate client process.
    let handle = section.duplicate_for(std::process::id()).unwrap();
    SharedIndexResponse {
        handle,
        len: bytes.len() as u64,
        generation,
        overlay: scry_core::delta::Delta::new(arena.len()).encode_query_overlay(),
    }
}

/// The whole point of `search_local_ordered` answering in-process is
/// defeated if it re-asks the daemon for a shared mapping on every call.
/// Once a mapping is established, repeated calls inside the staleness
/// window must reuse it rather than sending another `ShareIndex` frame.
#[test]
fn local_search_reuses_mapping_within_staleness_window() {
    let pipe_name = unique_pipe_name();
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        let mut share_index_requests = 0u32;
        // The client closes the pipe on drop; that ends this loop.
        while let Ok(request) = pipe.read_frame() {
            assert_eq!(
                decode_request(&request).unwrap().kind,
                QueryKind::ShareIndex
            );
            share_index_requests += 1;
            pipe.write_frame(&encode_shared_index(&shared_index_response(1)))
                .unwrap();
        }
        share_index_requests
    });

    let mut client = connect(&pipe_name);
    for _ in 0..5 {
        let (result, execution) = client
            .search_local_ordered_with_execution(QueryKind::Substring, "file", 50, Order::Relevance)
            .unwrap();
        assert_eq!(execution, SearchExecution::SharedIndex);
        assert_eq!(result.len(), 1);
    }
    drop(client);

    let share_index_requests = thread.join().unwrap();
    assert_eq!(
        share_index_requests, 1,
        "repeated calls within the staleness window must not re-handshake"
    );
}

/// When the daemon can't or won't share a mapping, `search_local` must
/// still answer — over a fresh RPC connection — rather than erroring out.
#[test]
fn local_search_falls_back_to_rpc_when_sharing_unavailable() {
    let pipe_name = unique_pipe_name();
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();
        let request = pipe.read_frame().unwrap();
        assert_eq!(
            decode_request(&request).unwrap().kind,
            QueryKind::ShareIndex
        );
        // Sharing unavailable: not a valid `SharedIndexResponse` frame.
        pipe.write_frame(b"not a shared index").unwrap();
        drop(pipe);

        // The client reconnects to retry over plain RPC.
        let pipe = server.accept().unwrap();
        let request = pipe.read_frame().unwrap();
        let decoded = decode_request(&request).unwrap();
        assert_eq!(decoded.kind, QueryKind::Substring);
        pipe.write_frame(&encode_results(&[entry("file.txt")]))
            .unwrap();
    });

    let mut client = connect(&pipe_name);
    let (result, execution) = client
        .search_local_ordered_with_execution(QueryKind::Substring, "file", 50, Order::Relevance)
        .unwrap();
    assert_eq!(execution, SearchExecution::RpcFallback);
    assert_eq!(result, vec![entry("file.txt")]);

    drop(client);
    thread.join().unwrap();
}

/// A rejected replacement is evidence that the cached generation is no
/// longer current. The fallback answer is safe, but retaining the old
/// mapping or refreshing the handshake timer would make subsequent calls
/// knowingly serve that stale generation.
#[test]
fn failed_replacement_mapping_discards_old_generation_and_keeps_retry_due() {
    let pipe_name = unique_pipe_name();
    let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
    let thread = std::thread::spawn(move || {
        let pipe = server.accept().unwrap();

        let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
        assert_eq!(request.kind, QueryKind::ShareIndex);
        pipe.write_frame(&encode_shared_index(&shared_index_response(1)))
            .unwrap();

        let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
        assert_eq!(request.kind, QueryKind::ShareIndex);
        pipe.write_frame(&encode_shared_index(&SharedIndexResponse {
            handle: u64::MAX,
            len: 4096,
            generation: 2,
            overlay: Vec::new(),
        }))
        .unwrap();

        let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
        assert_eq!(request.kind, QueryKind::Substring);
        pipe.write_frame(&encode_results(&[entry("rpc-result")]))
            .unwrap();
    });

    let mut client = connect(&pipe_name);
    assert_eq!(
        client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap()
            .len(),
        1
    );
    client.last_handshake = Some(std::time::Instant::now() - LOCAL_HANDSHAKE_STALENESS_WINDOW);

    let (result, execution) = client
        .search_local_ordered_with_execution(QueryKind::Substring, "file", 50, Order::Relevance)
        .unwrap();
    assert_eq!(execution, SearchExecution::RpcFallback);
    assert_eq!(result, vec![entry("rpc-result")]);
    assert!(client.local.is_none());
    assert!(client.last_handshake.unwrap().elapsed() >= LOCAL_HANDSHAKE_STALENESS_WINDOW);

    drop(client);
    thread.join().unwrap();
}
