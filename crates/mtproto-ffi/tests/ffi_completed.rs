//! `MTEventKindCompleted`'s flags through the C ABI (M8b's E-1): `MTCompletedChained` when the answered message
//! carried `invokeAfterMsg`, `MTCompletedSentAgain` when the request went out under more than one msg_id.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use mtproto_engine_ffi::*;
use mtproto_testserver::*;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Seen {
    kind: u32,
    request: u64,
    flags: u32,
}

#[derive(Default)]
struct Sink {
    events: Mutex<Vec<Seen>>,
    condvar: Condvar,
}

unsafe extern "C" fn on_event(context: *mut c_void, _session: u64, event: *const MTEvent) {
    let sink = unsafe { &*(context as *const Sink) };
    let event = unsafe { &*event };
    if !event.payload.is_null() {
        unsafe { mt_buffer_free(event.payload) };
    }
    sink.events.lock().unwrap().push(Seen { kind: event.kind, request: event.request_id, flags: event.flags });
    sink.condvar.notify_all();
}

impl Sink {
    fn wait(&self, timeout: Duration, predicate: impl Fn(&[Seen]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut events = self.events.lock().unwrap();
        while !predicate(&events) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            events = self.condvar.wait_timeout(events, deadline - now).unwrap().0;
        }
        true
    }
}

fn bytes(data: &[u8]) -> MTBytes {
    MTBytes { data: data.as_ptr(), length: data.len() }
}

fn string(text: &str) -> MTString {
    MTString { data: text.as_ptr(), length: text.len() }
}

const KIND_COMPLETED: u32 = 1;

#[test]
fn a_dependent_completes_chained_and_its_dependency_does_not() {
    let key = random_key(93);
    let server = TestServer::start(vec![key.clone()], ServerOptions::default());
    let sink: &'static Sink = Box::leak(Box::default());
    let engine = unsafe { mt_engine_create(1, sink as *const Sink as *mut c_void, Some(on_event), None) };
    assert!(!engine.is_null());
    let host = server.address.ip().to_string();
    let address = MTAddress { host: string(&host), port: server.address.port(), secret: bytes(&[]) };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let salt = MTSaltEntry { salt: SERVER_SALT, valid_since: now - 60.0, valid_until: now + 3600.0 };
    let setup = MTSessionSetup {
        datacenter_id: 2,
        obfuscation_dc_id: 2,
        role: 0,
        framing: 0,
        addresses: &address,
        address_count: 1,
        proxy: MTProxy {
            kind: 0,
            host: string(""),
            port: 0,
            username: string(""),
            password: string(""),
            secret: bytes(&[]),
        },
        auth_key: bytes(key.bytes()),
        salts: &salt,
        salt_count: 1,
        has_init_hash: 0,
        init_hash: string(""),
        generate_key: 0,
        public_keys_pem: std::ptr::null(),
        public_key_count: 0,
        temp_key_expires_in: 0,
        environment: std::ptr::null(),
        time_difference: 0.0,
        online: 0,
        paused: 0,
        keep_connected: 1,
        idle_disconnect_after: 0.0,
        request_timeout: 5.0,
        pfs_lifetime: 0,
        pfs_make_permanent_key: 0,
        pfs_temporary_key: std::ptr::null(),
    };
    let session = unsafe { mt_session_create(engine, &setup) };
    assert_ne!(session, 0);
    let send = |tag: u32, invoke_after: u64| {
        let id = unsafe { mt_engine_next_request_id(engine) };
        let body = call(tag, &id.to_le_bytes());
        let request = MTRequest { id, body: bytes(&body), flags: 0, expected_response_size: 0, invoke_after };
        unsafe { mt_session_send(engine, session, &request) };
        id
    };
    let first = send(7, 0);
    let second = send(7, first);
    assert!(sink.wait(Duration::from_secs(10), |events| {
        [first, second].iter().all(|id| events.iter().any(|e| e.kind == KIND_COMPLETED && e.request == *id))
    }));
    let events = sink.events.lock().unwrap().clone();
    let completed = |id: u64| events.iter().find(|e| e.kind == KIND_COMPLETED && e.request == id).unwrap().flags;
    assert_eq!(completed(first), 0, "went out once, not chained");
    assert_eq!(completed(second), COMPLETED_CHAINED, "went out once, chained to the first");
    unsafe { mt_session_destroy(engine, session) };
    unsafe { mt_engine_destroy(engine) };
}

#[test]
fn the_header_declares_the_completed_flags() {
    let header = include_str!("../include/mtproto_engine.h");
    assert!(header.contains("MTCompletedSentAgain = 1 << 0,"));
    assert!(header.contains("MTCompletedChained = 1 << 1,"));
    assert_eq!(COMPLETED_SENT_AGAIN, 1);
    assert_eq!(COMPLETED_CHAINED, 2);
}
