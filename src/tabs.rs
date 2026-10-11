//! Dock tool tabs: pod logs, terminals (local shell / pod exec / drain) and the YAML editor.
use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, RichText, TextStyle};
use egui_term::{BackendSettings, FontSettings, PtyEvent, TerminalBackend, TerminalFont, TerminalView};
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ApiResource, LogParams};
use kube::Client;
use serde_json::Value;

use crate::find::{self, Find, Matcher};
use crate::kubeconfig::{self, Ctx};
use crate::ops::{self, Kind, Res};
use crate::watch::{take, Bg, Lock, Pending, GREEN, ORANGE, RED};

mod logs;
mod term;
mod yaml;

pub use logs::{LogState, LogTab};
pub use term::{drain, kubectl_line, local_shell, node_exec, pod_shell, set_scrollback, Cleanup, Command, TermTab, TERM_TX};
pub use yaml::YamlTab;
use term::TERM_KEYS;
