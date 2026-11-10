//! Asynchronous SQLite event log for SEED SAC.
//!
//! Database access belongs to the dedicated `memory-writer` thread. Callers
//! send events through an unbounded standard-library channel, so neither the
//! HUD nor the sentinel sampler performs SQLite I/O.

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use rusqlite::{Connection, params};

/// Event categories recorded by the system event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    YellowEnter,
    RedEnter,
    SigstopSent,
    SigcontSent,
    Terminated,
}

impl EventType {
    /// Stable database representation for this event category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::YellowEnter => "YELLOW_ENTER",
            Self::RedEnter => "RED_ENTER",
            Self::SigstopSent => "SIGSTOP_SENT",
            Self::SigcontSent => "SIGCONT_SENT",
            Self::Terminated => "TERMINATED",
        }
    }
}

/// One system observation to append to the SQLite event log.
#[derive(Debug, Clone)]
pub struct SystemEvent {
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    pub event_type: EventType,
    pub target_pid: Option<u32>,
    pub process_name: Option<String>,
    pub ram_usage_mb: Option<f64>,
    pub cpu_usage_pct: Option<f64>,
    pub vram_usage_mb: Option<f64>,
    pub details: Option<String>,
}

enum Command {
    Record(SystemEvent),
    Shutdown,
}

/// Errors returned while starting or stopping the event writer.
#[derive(Debug)]
pub enum MemoryError {
    WorkerStart(String),
    WorkerStopped,
    WorkerPanicked,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerStart(message) => write!(f, "could not start memory writer: {message}"),
            Self::WorkerStopped => f.write_str("memory writer is no longer available"),
            Self::WorkerPanicked => f.write_str("memory writer thread panicked"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// Handle for enqueueing events without performing database work on the caller.
pub struct EventWriter {
    sender: Option<Sender<Command>>,
    worker: Option<JoinHandle<()>>,
}

impl EventWriter {
    /// Start the dedicated SQLite writer and initialize the database schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_path_buf();

        if let Err(error) = ensure_database_dir(&path) {
            return Err(MemoryError::WorkerStart(format!(
                "could not create database directory: {error}"
            )));
        }

        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let worker = thread::Builder::new()
            .name("memory-writer".to_owned())
            .spawn(move || {
                let connection = match initialize_database(&path) {
                    Ok(connection) => {
                        let _ = ready_tx.send(Ok(()));
                        connection
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };

                run_writer(connection, receiver);
            })
            .map_err(|error| MemoryError::WorkerStart(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                sender: Some(sender),
                worker: Some(worker),
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(MemoryError::WorkerStart(message))
            }
            Err(error) => {
                let _ = worker.join();
                Err(MemoryError::WorkerStart(error.to_string()))
            }
        }
    }

    /// Queue an event for persistence. This only sends a message to the worker.
    pub fn record(&self, event: SystemEvent) -> Result<(), MemoryError> {
        self.sender
            .as_ref()
            .expect("writer sender is present until shutdown")
            .send(Command::Record(event))
            .map_err(|_| MemoryError::WorkerStopped)
    }

    /// Close the channel and wait for queued events to be written.
    pub fn shutdown(mut self) -> Result<(), MemoryError> {
        self.stop_worker();
        match self
            .worker
            .take()
            .expect("worker is present until shutdown")
            .join()
        {
            Ok(()) => Ok(()),
            Err(_) => Err(MemoryError::WorkerPanicked),
        }
    }

    fn stop_worker(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Command::Shutdown);
        }
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        // Dropping does not wait for database I/O; the detached worker drains
        // queued events and exits after it receives Shutdown.
        self.stop_worker();
    }
}

fn initialize_database(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS system_events (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             timestamp INTEGER NOT NULL,
             event_type TEXT NOT NULL,
             target_pid INTEGER,
             process_name TEXT,
             ram_usage_mb REAL,
             cpu_usage_pct REAL,
             vram_usage_mb REAL,
             details TEXT
         );",
    )?;
    Ok(connection)
}

fn run_writer(connection: Connection, receiver: mpsc::Receiver<Command>) {
    let mut statement = match connection.prepare(
        "INSERT INTO system_events (
             timestamp, event_type, target_pid, process_name,
             ram_usage_mb, cpu_usage_pct, vram_usage_mb, details
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    ) {
        Ok(statement) => statement,
        Err(error) => {
            eprintln!("SEED SAC memory writer could not prepare insert: {error}");
            return;
        }
    };

    while let Ok(command) = receiver.recv() {
        match command {
            Command::Record(event) => {
                if let Err(error) = statement.execute(params![
                    event.timestamp,
                    event.event_type.as_str(),
                    event.target_pid,
                    event.process_name,
                    event.ram_usage_mb,
                    event.cpu_usage_pct,
                    event.vram_usage_mb,
                    event.details,
                ]) {
                    eprintln!("SEED SAC memory writer could not record event: {error}");
                }
            }
            Command::Shutdown => break,
        }
    }
}

/// Get the default XDG data directory path for SEED SAC events.db.
/// Falls back to ~/.local/share/seed-sac if XDG_DATA_HOME is not set.
pub fn default_database_path() -> std::path::PathBuf {
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        let mut path = std::path::PathBuf::from(xdg_data);
        path.push("seed-sac");
        path
    } else if let Ok(home) = std::env::var("HOME") {
        let mut path = std::path::PathBuf::from(home);
        path.push(".local/share/seed-sac");
        path
    } else {
        std::path::PathBuf::from("./.local/seed-sac")
    }
}

/// Ensure the database directory exists, creating it if necessary.
fn ensure_database_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(EventType::YellowEnter.as_str(), "YELLOW_ENTER");
        assert_eq!(EventType::RedEnter.as_str(), "RED_ENTER");
        assert_eq!(EventType::SigstopSent.as_str(), "SIGSTOP_SENT");
        assert_eq!(EventType::SigcontSent.as_str(), "SIGCONT_SENT");
        assert_eq!(EventType::Terminated.as_str(), "TERMINATED");
    }

    #[test]
    fn test_event_writer_in_memory() {
        let writer = EventWriter::open(":memory:").expect("in-memory database failed");

        let event = SystemEvent {
            timestamp: 1234567890,
            event_type: EventType::YellowEnter,
            target_pid: Some(1234),
            process_name: Some("test-process".to_owned()),
            ram_usage_mb: Some(256.5),
            cpu_usage_pct: Some(50.0),
            vram_usage_mb: Some(128.0),
            details: Some("test details".to_owned()),
        };

        writer.record(event).expect("record failed");

        writer.shutdown().expect("shutdown failed");
    }

    #[test]
    fn test_event_writer_multiple_events() {
        let writer = EventWriter::open(":memory:").expect("in-memory database failed");

        for i in 0..10 {
            let event = SystemEvent {
                timestamp: 1000000 + i * 1000,
                event_type: if i % 2 == 0 {
                    EventType::YellowEnter
                } else {
                    EventType::RedEnter
                },
                target_pid: Some(2000 + i as u32),
                process_name: Some(format!("process-{i}")),
                ram_usage_mb: Some(100.0 + i as f64 * 10.0),
                cpu_usage_pct: Some(50.0),
                vram_usage_mb: None,
                details: None,
            };
            writer.record(event).expect("record failed");
        }

        writer.shutdown().expect("shutdown failed");
    }

    #[test]
    fn test_event_writer_with_tempfile() {
        let tempdir = tempfile::TempDir::new().expect("tempdir failed");
        let db_path = tempdir.path().join("events.db");

        {
            let writer = EventWriter::open(&db_path).expect("open database failed");

            let event = SystemEvent {
                timestamp: 1111111111,
                event_type: EventType::SigstopSent,
                target_pid: Some(5555),
                process_name: Some("linker".to_owned()),
                ram_usage_mb: Some(512.0),
                cpu_usage_pct: Some(95.0),
                vram_usage_mb: Some(256.0),
                details: Some("memory spike detected".to_owned()),
            };

            writer.record(event).expect("record failed");
            writer.shutdown().expect("shutdown failed");
        }

        // Verify file was created
        assert!(db_path.exists(), "database file was not created");
        let metadata = std::fs::metadata(&db_path).expect("metadata failed");
        assert!(metadata.len() > 0, "database file is empty");
    }

    #[test]
    fn test_event_writer_drop_behavior() {
        let writer = EventWriter::open(":memory:").expect("in-memory database failed");

        let event = SystemEvent {
            timestamp: 9999999999,
            event_type: EventType::Terminated,
            target_pid: Some(9999),
            process_name: None,
            ram_usage_mb: None,
            cpu_usage_pct: None,
            vram_usage_mb: None,
            details: None,
        };

        writer.record(event).expect("record failed");
        // Drop without explicit shutdown — worker thread should drain the queue
        drop(writer);
    }

    #[test]
    fn test_system_event_all_optional_fields() {
        let event = SystemEvent {
            timestamp: 123456789,
            event_type: EventType::SigcontSent,
            target_pid: None,
            process_name: None,
            ram_usage_mb: None,
            cpu_usage_pct: None,
            vram_usage_mb: None,
            details: None,
        };

        assert_eq!(event.timestamp, 123456789);
        assert_eq!(event.event_type, EventType::SigcontSent);
        assert!(event.target_pid.is_none());
    }

    #[test]
    fn test_default_database_path_with_xdg() {
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-data") };
        let path = default_database_path();
        assert!(path.to_string_lossy().contains("xdg-data"));
        assert!(path.to_string_lossy().contains("seed-sac"));
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
    }

    #[test]
    fn test_default_database_path_without_xdg() {
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        let path = default_database_path();
        assert!(
            path.to_string_lossy().contains(".local/share/seed-sac")
                || path.to_string_lossy().contains("seed-sac")
        );
    }
}
