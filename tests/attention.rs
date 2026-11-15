//! Attention protocol15+ and captured-target protocol17 acceptance use real hosted scripted
//! fake agents. Two logical hosts share one physical machine; provider quota,
//! physical network throughput and installed-client activation are not measured.
//! Set CEPHEUS_ATTENTION_TEST_BIN and CEPHEUS_ATTENTION_TEST_SOURCE to the exact
//! centrally built backend; retain its build receipt with emitted input hashes.
//! cargo test --test attention -- --show-output --test-threads=1
#[path = "attention/compatibility.rs"]
mod compatibility;
#[path = "attention/decisions.rs"]
mod decisions;
#[path = "attention/delivery.rs"]
mod delivery;
#[path = "job_sessions/support.rs"]
mod jobs_support;
#[path = "attention/manual.rs"]
mod manual;
#[path = "attention/native_permission.rs"]
mod native_permission;
#[path = "attention/supervisor.rs"]
mod supervisor;
#[path = "attention/support.rs"]
mod support;
#[path = "attention/target.rs"]
mod target;
use jobs_support::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use support::*;
