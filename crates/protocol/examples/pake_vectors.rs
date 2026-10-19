//! Test vectors for the web console's SPAKE2 (server/web/src/lib/spake2.ts):
//! the Rust side of an exchange with a deterministic browser message.
//!
//!   cargo run --example pake_vectors --features crypto -- <code> <browser message>
use farwing_protocol::crypto;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (code, browser) = (&args[1], &args[2]);
    let (state, host_msg) = crypto::pake_start(code);
    let key = state.finish(browser).expect("finish");
    println!("host_message={host_msg}");
    println!("key={}", crypto::b64(&key));
    println!("host_confirm={}", crypto::key_confirm(&key, "host", "rid-1"));
    println!("controller_confirm={}", crypto::key_confirm(&key, "controller", "rid-1"));
    let prints = vec!["sha-256 01:02:03".to_string(), "sha-256 aa:bb:cc".to_string()];
    println!("binding={}", crypto::pake_fingerprint_binding(&key, "sid-1", "controller", &prints));
    let (wrong, wrong_msg) = crypto::pake_start("000000");
    let wrong_key = wrong.finish(browser).expect("finish");
    println!("wrong_message={wrong_msg}");
    println!("wrong_key={}", crypto::b64(&wrong_key));
    let device = crypto::DeviceKey::generate();
    let public = device.public_key();
    println!("public_key={public}");
    println!("device_id={}", crypto::local_device_id(&public).unwrap());
}
