use crate::{daemon::session_id, harness::ChatMessage};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

const CONVERSATIONS_DIR: &str = "conversations";
const CONVERSATION_EXT: &str = "jsonl";
const EVENT_VERSION: u8 = 1;

pub(crate) enum ConversationSelection {
    New,
    ResumeLatest,
    Resume(String),
}

pub(crate) enum ConversationOrigin {
    New,
    Resumed,
}

pub(crate) struct ConversationStore {
    dir: PathBuf,
}

pub(crate) struct ConversationSession {
    id: String,
    path: PathBuf,
    messages: Vec<ChatMessage>,
    origin: ConversationOrigin,
}

#[derive(Serialize, Deserialize)]
struct ConversationEvent {
    version: u8,
    message: ChatMessage,
}

impl ConversationStore {
    pub(crate) fn new(runtime_dir: &Path) -> Self {
        Self {
            dir: runtime_dir.join(CONVERSATIONS_DIR),
        }
    }

    pub(crate) fn open(&self, selection: ConversationSelection) -> Result<ConversationSession> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))?;

        match selection {
            ConversationSelection::New => self.create_session(),
            ConversationSelection::ResumeLatest => match self.latest_session_path()? {
                Some(path) => self.resume_session(path),
                None => self.create_session(),
            },
            ConversationSelection::Resume(id) => {
                validate_session_id(&id)?;
                let path = self.session_path(&id);
                if !path.exists() {
                    bail!("conversation session {id} does not exist");
                }
                self.resume_session(path)
            }
        }
    }

    fn create_session(&self) -> Result<ConversationSession> {
        let id = session_id();
        let path = self.session_path(&id);
        File::create_new(&path)
            .with_context(|| format!("failed to create conversation {}", path.display()))?;
        Ok(ConversationSession {
            id,
            path,
            messages: Vec::new(),
            origin: ConversationOrigin::New,
        })
    }

    fn resume_session(&self, path: PathBuf) -> Result<ConversationSession> {
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("conversation file has no UTF-8 session ID")?
            .to_owned();
        let messages = load_messages(&path)?;
        Ok(ConversationSession {
            id,
            path,
            messages,
            origin: ConversationOrigin::Resumed,
        })
    }

    fn latest_session_path(&self) -> Result<Option<PathBuf>> {
        let mut latest = None;
        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read {}", self.dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some(CONVERSATION_EXT) {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match latest {
                Some((current, _)) if current >= modified => {}
                _ => latest = Some((modified, path)),
            }
        }
        Ok(latest.map(|(_, path)| path))
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{CONVERSATION_EXT}"))
    }
}

impl ConversationSession {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn origin(&self) -> &ConversationOrigin {
        &self.origin
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) fn append_message(&mut self, message: &ChatMessage) -> Result<()> {
        if message.is_system() {
            return Ok(());
        }

        let event = ConversationEvent {
            version: EVENT_VERSION,
            message: message.clone(),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        serde_json::to_writer(&mut file, &event)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.messages.push(message.clone());
        Ok(())
    }
}

fn load_messages(path: &Path) -> Result<Vec<ChatMessage>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let lines = BufReader::new(file)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", path.display()))?;
    let last_non_empty = lines.iter().rposition(|line| !line.trim().is_empty());
    let mut messages = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<ConversationEvent>(line) {
            Ok(event) => event,
            Err(error) if Some(index) == last_non_empty => {
                eprintln!(
                    "ignoring trailing malformed conversation event in {}: {error}",
                    path.display()
                );
                break;
            }
            Err(error) => {
                bail!(
                    "invalid conversation event in {} at line {}: {error}",
                    path.display(),
                    index + 1
                );
            }
        };
        if event.version != EVENT_VERSION {
            bail!(
                "unsupported conversation event version {} in {} at line {}",
                event.version,
                path.display(),
                index + 1
            );
        }
        if !event.message.is_system() {
            messages.push(event.message);
        }
    }

    Ok(messages)
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid conversation session ID");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ConversationOrigin, ConversationSelection, ConversationStore};
    use crate::harness::ChatMessage;
    use std::{fs, io::Write, path::PathBuf};

    #[test]
    fn appends_and_loads_messages_in_order() {
        let root = temp_root("order");
        let store = ConversationStore::new(&root);
        let mut session = store.open(ConversationSelection::New).unwrap();
        session.append_message(&ChatMessage::user("hello")).unwrap();
        session
            .append_message(&ChatMessage::assistant_text("hi"))
            .unwrap();

        let resumed = store
            .open(ConversationSelection::Resume(session.id().to_owned()))
            .unwrap();
        let texts = resumed
            .messages()
            .iter()
            .map(ChatMessage::content_text)
            .collect::<Vec<_>>();

        assert!(matches!(resumed.origin(), ConversationOrigin::Resumed));
        assert_eq!(texts, vec!["hello", "hi"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skips_system_messages() {
        let root = temp_root("system");
        let store = ConversationStore::new(&root);
        let mut session = store.open(ConversationSelection::New).unwrap();
        session
            .append_message(&ChatMessage::system("current status"))
            .unwrap();
        session.append_message(&ChatMessage::user("hello")).unwrap();

        let resumed = store
            .open(ConversationSelection::Resume(session.id().to_owned()))
            .unwrap();

        assert_eq!(resumed.messages().len(), 1);
        assert_eq!(resumed.messages()[0].content_text(), "hello");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tolerates_malformed_trailing_line() {
        let root = temp_root("trailing");
        let store = ConversationStore::new(&root);
        let mut session = store.open(ConversationSelection::New).unwrap();
        session.append_message(&ChatMessage::user("hello")).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(
                root.join("conversations")
                    .join(format!("{}.jsonl", session.id())),
            )
            .unwrap()
            .write_all(b"{not-json")
            .unwrap();

        let resumed = store
            .open(ConversationSelection::Resume(session.id().to_owned()))
            .unwrap();

        assert_eq!(resumed.messages().len(), 1);
        assert_eq!(resumed.messages()[0].content_text(), "hello");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_malformed_middle_line() {
        let root = temp_root("middle");
        let store = ConversationStore::new(&root);
        let mut session = store.open(ConversationSelection::New).unwrap();
        session.append_message(&ChatMessage::user("hello")).unwrap();
        let path = root
            .join("conversations")
            .join(format!("{}.jsonl", session.id()));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{not-json\n")
            .unwrap();
        session
            .append_message(&ChatMessage::assistant_text("hi"))
            .unwrap();

        let error = match store.open(ConversationSelection::Resume(session.id().to_owned())) {
            Ok(_) => panic!("malformed middle line should fail"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("invalid conversation event"));
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "emissary-conversation-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
