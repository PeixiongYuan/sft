use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::askpass::PasswordHandle;
use crate::browser::{self, Entry, Pane, SortMode};
use crate::history::{HostHistory, HostStats};
use crate::path_store::{PathEntry, PathStore};
use crate::remote::{self, Connection};
use crate::ssh_config::{HostDraft, SshConfig};

#[derive(Debug, Clone)]
pub struct PathCompletion {
    pub replacement: Option<String>,
    pub candidates: Vec<String>,
}

#[derive(Debug)]
pub enum AppEvent {
    HostChecked {
        host: String,
        status: HostAvailability,
    },
    RemoteListed {
        path: String,
        result: Result<Vec<Entry>, String>,
    },
    PathCompleted {
        pane: Pane,
        input: String,
        result: Result<Option<PathCompletion>, String>,
    },
    TransferFinished {
        result: Result<(), String>,
    },
    FileOperationFinished {
        operation: FileOperation,
        result: Result<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAvailability {
    Unknown,
    Checking,
    Reachable { rtt_ms: u32 },
    Slow { rtt_ms: u32 },
    Unreachable,
}

impl HostAvailability {
    fn from_rtt(rtt_ms: Option<u32>) -> Self {
        match rtt_ms {
            Some(ms) if ms >= remote::SLOW_REACHABILITY_MS => Self::Slow { rtt_ms: ms },
            Some(ms) => Self::Reachable { rtt_ms: ms },
            None => Self::Unreachable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Hosts,
    Files,
    HostForm,
    DeleteHost,
    PasswordPrompt,
    PathPrompt,
    SearchPrompt,
    PathList,
    NamePrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Mkdir,
    Rename,
    Delete,
}

impl FileOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Mkdir => "create directory",
            Self::Rename => "rename",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFormMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostField {
    Alias,
    HostName,
    User,
    Port,
    IdentityFile,
}

impl HostField {
    pub const ALL: [Self; 5] = [
        Self::Alias,
        Self::HostName,
        Self::User,
        Self::Port,
        Self::IdentityFile,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Alias => "Host",
            Self::HostName => "HostName",
            Self::User => "User",
            Self::Port => "Port",
            Self::IdentityFile => "IdentityFile",
        }
    }

    fn next(self) -> Self {
        let idx = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let idx = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone)]
pub struct HostForm {
    pub mode: HostFormMode,
    pub original_alias: Option<String>,
    pub draft: HostDraft,
    pub field: HostField,
}

#[derive(Debug, Clone)]
pub struct PasswordPrompt {
    pub host: String,
    pub input: String,
    pub return_to: Screen,
    pub open_after_save: bool,
}

#[derive(Debug, Clone)]
pub struct PathPrompt {
    pub pane: Pane,
    pub current: String,
    pub input: String,
    pub completing: bool,
    pub completions: Vec<String>,
    pub completion_selected: usize,
}

#[derive(Debug, Clone)]
pub struct SearchPrompt {
    pub pane: Pane,
    pub input: String,
    pub previous: String,
}

#[derive(Debug, Clone)]
pub struct NamePrompt {
    pub pane: Pane,
    pub kind: NamePromptKind,
    pub input: String,
}

#[derive(Debug, Clone)]
pub enum NamePromptKind {
    Mkdir,
    Rename { old_name: String },
}

#[derive(Debug, Clone)]
pub struct DeletePlan {
    pub pane: Pane,
    pub names: Vec<String>,
    pub has_dirs: bool,
}

impl HostForm {
    fn add() -> Self {
        Self {
            mode: HostFormMode::Add,
            original_alias: None,
            draft: HostDraft::default(),
            field: HostField::Alias,
        }
    }

    fn edit(alias: String, draft: HostDraft) -> Self {
        Self {
            mode: HostFormMode::Edit,
            original_alias: Some(alias),
            draft,
            field: HostField::Alias,
        }
    }

    pub fn value(&self, field: HostField) -> &str {
        match field {
            HostField::Alias => &self.draft.alias,
            HostField::HostName => &self.draft.hostname,
            HostField::User => &self.draft.user,
            HostField::Port => &self.draft.port,
            HostField::IdentityFile => &self.draft.identity_file,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self.field {
            HostField::Alias => &mut self.draft.alias,
            HostField::HostName => &mut self.draft.hostname,
            HostField::User => &mut self.draft.user,
            HostField::Port => &mut self.draft.port,
            HostField::IdentityFile => &mut self.draft.identity_file,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CopyPlan {
    pub source: Pane,
    pub names: Vec<String>,
    pub has_dirs: bool,
}

#[derive(Debug)]
pub struct FileSession {
    pub connection: Connection,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub remote_home: String,
    pub local_entries: Vec<Entry>,
    pub remote_entries: Vec<Entry>,
    pub local_state: ListState,
    pub remote_state: ListState,
    pub local_selected: HashSet<String>,
    pub remote_selected: HashSet<String>,
    pub active: Pane,
    pub show_hidden: bool,
    pub sort: SortMode,
    pub local_filter: String,
    pub remote_filter: String,
    pub local_error: Option<String>,
    pub remote_error: Option<String>,
    pub remote_loading: bool,
    pub confirm: Option<CopyPlan>,
    pub delete_confirm: Option<DeletePlan>,
    pub transferring: Option<String>,
}

impl FileSession {
    fn new(
        connection: Connection,
        local_path: PathBuf,
        remote_path: String,
        remote_home: String,
        show_hidden: bool,
    ) -> Result<Self> {
        let mut local_state = ListState::default();
        local_state.select(Some(0));
        let mut remote_state = ListState::default();
        remote_state.select(Some(0));

        let mut session = Self {
            connection,
            local_path,
            remote_path,
            remote_home,
            local_entries: Vec::new(),
            remote_entries: Vec::new(),
            local_state,
            remote_state,
            local_selected: HashSet::new(),
            remote_selected: HashSet::new(),
            active: Pane::Local,
            show_hidden,
            sort: SortMode::Name,
            local_filter: String::new(),
            remote_filter: String::new(),
            local_error: None,
            remote_error: None,
            remote_loading: true,
            confirm: None,
            delete_confirm: None,
            transferring: None,
        };
        session
            .refresh_local()
            .with_context(|| format!("failed to read {}", session.local_path.display()))?;
        Ok(session)
    }

    fn refresh_local(&mut self) -> Result<()> {
        match browser::list_local(&self.local_path, self.show_hidden, self.sort) {
            Ok(entries) => {
                self.local_entries = entries;
                self.local_error = None;
                self.local_state.select(Some(0));
                self.local_selected.clear();
            }
            Err(err) => {
                self.local_entries.clear();
                self.local_error = Some(err.to_string());
            }
        }
        Ok(())
    }

    fn request_remote_listing(&mut self, path: String, tx: Sender<AppEvent>) {
        self.remote_path = path.clone();
        self.remote_loading = true;
        self.remote_error = None;
        self.remote_selected.clear();
        self.remote_state.select(Some(0));

        let connection = self.connection.clone();
        let show_hidden = self.show_hidden;
        let sort = self.sort;
        thread::spawn(move || {
            let result = remote::fetch_remote_listing(&connection, &path, show_hidden, sort);
            let _ = tx.send(AppEvent::RemoteListed { path, result });
        });
    }

    fn selected_index(&self) -> usize {
        match self.active {
            Pane::Local => self.local_state.selected().unwrap_or(0),
            Pane::Remote => self.remote_state.selected().unwrap_or(0),
        }
    }

    fn active_len(&self) -> usize {
        match self.active {
            Pane::Local => {
                browser::filter_entries(&self.local_entries, &self.local_filter).len() + 1
            }
            Pane::Remote => {
                browser::filter_entries(&self.remote_entries, &self.remote_filter).len() + 1
            }
        }
    }

    fn active_state(&mut self) -> &mut ListState {
        match self.active {
            Pane::Local => &mut self.local_state,
            Pane::Remote => &mut self.remote_state,
        }
    }

    fn filter_for(&self, pane: Pane) -> &str {
        match pane {
            Pane::Local => &self.local_filter,
            Pane::Remote => &self.remote_filter,
        }
    }

    fn set_filter(&mut self, pane: Pane, value: String) {
        match pane {
            Pane::Local => {
                self.local_filter = value;
                self.local_state.select(Some(0));
            }
            Pane::Remote => {
                self.remote_filter = value;
                self.remote_state.select(Some(0));
            }
        }
    }
}

#[derive(Debug)]
pub struct App {
    pub ssh_config: SshConfig,
    pub history: HostHistory,
    pub path_store: PathStore,
    pub availability: HashMap<String, HostAvailability>,
    pub host_state: ListState,
    pub path_state: ListState,
    pub screen: Screen,
    pub form: Option<HostForm>,
    pub password_prompt: Option<PasswordPrompt>,
    pub path_prompt: Option<PathPrompt>,
    pub search_prompt: Option<SearchPrompt>,
    pub name_prompt: Option<NamePrompt>,
    pub file: Option<FileSession>,
    pub passwords: HashMap<String, PasswordHandle>,
    pub local_start: PathBuf,
    pub remote_start: Option<String>,
    pub show_hidden: bool,
    pub message: Option<String>,
    pub should_quit: bool,
    direct_host: bool,
}

impl App {
    pub fn new(
        config_path: PathBuf,
        initial_host: Option<String>,
        local_path: PathBuf,
        remote_path: Option<String>,
        show_hidden: bool,
    ) -> Result<Self> {
        let ssh_config = SshConfig::load(config_path)?;
        let history = HostHistory::load_default();
        let path_store = PathStore::load_default();
        let mut host_state = ListState::default();
        host_state.select(Some(0));
        let mut path_state = ListState::default();
        path_state.select(Some(0));
        let mut app = Self {
            ssh_config,
            history,
            path_store,
            availability: HashMap::new(),
            host_state,
            path_state,
            screen: Screen::Hosts,
            form: None,
            password_prompt: None,
            path_prompt: None,
            search_prompt: None,
            name_prompt: None,
            file: None,
            passwords: HashMap::new(),
            local_start: local_path,
            remote_start: remote_path,
            show_hidden,
            message: None,
            should_quit: false,
            direct_host: initial_host.is_some(),
        };

        if let Some(host) = initial_host {
            app.prompt_for_host_password(host, Screen::Hosts, true);
        }
        Ok(app)
    }

    pub fn start(&mut self, tx: Sender<AppEvent>) {
        if !matches!(self.screen, Screen::Files) {
            self.request_host_checks(tx.clone());
        }
        if let Some(file) = self.file.as_mut() {
            file.request_remote_listing(file.remote_path.clone(), tx);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        match self.screen {
            Screen::Hosts => self.handle_hosts_key(key, tx),
            Screen::Files => self.handle_files_key(key, tx),
            Screen::HostForm => self.handle_form_key(key),
            Screen::DeleteHost => self.handle_delete_key(key),
            Screen::PasswordPrompt => self.handle_password_key(key, tx),
            Screen::PathPrompt => self.handle_path_prompt_key(key, tx),
            Screen::SearchPrompt => self.handle_search_prompt_key(key),
            Screen::PathList => self.handle_path_list_key(key, tx),
            Screen::NamePrompt => self.handle_name_prompt_key(key, tx),
        }
    }

    pub fn handle_event(&mut self, event: AppEvent, tx: Sender<AppEvent>) -> Result<()> {
        match event {
            AppEvent::HostChecked { host, status } => {
                self.availability.insert(host, status);
            }
            AppEvent::RemoteListed { path, result } => {
                let mut auth_prompt_host = None;
                let Some(file) = self.file.as_mut() else {
                    return Ok(());
                };
                if path != file.remote_path {
                    return Ok(());
                }
                file.remote_loading = false;
                match result {
                    Ok(entries) => {
                        self.availability
                            .entry(file.connection.host.clone())
                            .or_insert(HostAvailability::Reachable { rtt_ms: 0 });
                        file.remote_entries = entries;
                        file.remote_error = None;
                        file.remote_state.select(Some(0));
                    }
                    Err(err) => {
                        if is_auth_error(&err) {
                            auth_prompt_host = Some(file.connection.host.clone());
                        }
                        file.remote_entries.clear();
                        file.remote_error = Some(err);
                    }
                }
                if let Some(host) = auth_prompt_host {
                    self.prompt_for_host_password(host.clone(), Screen::Files, false);
                    self.message = Some(format!("authentication required for {host}"));
                }
            }
            AppEvent::PathCompleted {
                pane,
                input,
                result,
            } => {
                let Some(prompt) = self.path_prompt.as_mut() else {
                    return Ok(());
                };
                if prompt.pane != pane || prompt.input != input {
                    return Ok(());
                }
                prompt.completing = false;
                prompt.completions.clear();
                prompt.completion_selected = 0;
                match result {
                    Ok(Some(completion)) => {
                        if let Some(replacement) = completion.replacement {
                            prompt.input = replacement;
                        }
                        prompt.completions = completion.candidates;
                        prompt.completion_selected = 0;
                        self.message = None;
                    }
                    Ok(None) => {
                        self.message = Some("no directory completion".to_string());
                    }
                    Err(err) => {
                        self.message = Some(format!("completion failed: {err}"));
                    }
                }
            }
            AppEvent::TransferFinished { result } => {
                let mut auth_prompt_host = None;
                if let Some(file) = self.file.as_mut() {
                    file.transferring = None;
                }
                match result {
                    Ok(()) => {
                        self.refresh_file_panes(tx)?;
                        self.message =
                            Some("transfer complete; refreshed local and remote".to_string());
                    }
                    Err(err) => {
                        if is_auth_error(&err) {
                            auth_prompt_host =
                                self.file.as_ref().map(|file| file.connection.host.clone());
                        }
                        self.message = Some(format!("transfer failed: {err}"));
                    }
                }
                if let Some(host) = auth_prompt_host {
                    self.prompt_for_host_password(host.clone(), Screen::Files, false);
                    self.message = Some(format!("authentication required for {host}"));
                }
            }
            AppEvent::FileOperationFinished { operation, result } => {
                let mut auth_prompt_host = None;
                if let Some(file) = self.file.as_mut() {
                    file.transferring = None;
                }
                match result {
                    Ok(message) => {
                        self.refresh_file_panes(tx)?;
                        self.message = Some(message);
                    }
                    Err(err) => {
                        if is_auth_error(&err) {
                            auth_prompt_host =
                                self.file.as_ref().map(|file| file.connection.host.clone());
                        }
                        self.message = Some(format!("{} failed: {err}", operation.label()));
                    }
                }
                if let Some(host) = auth_prompt_host {
                    self.prompt_for_host_password(host.clone(), Screen::Files, false);
                    self.message = Some(format!("authentication required for {host}"));
                }
            }
        }
        Ok(())
    }

    fn refresh_file_panes(&mut self, tx: Sender<AppEvent>) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.refresh_local()?;
        file.request_remote_listing(file.remote_path.clone(), tx);
        self.message = Some("refreshed local and remote".to_string());
        Ok(())
    }

    fn handle_hosts_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_host_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_host_selection(-1),
            KeyCode::PageDown => self.move_host_selection(10),
            KeyCode::PageUp => self.move_host_selection(-10),
            KeyCode::Enter => {
                if let Some(alias) = self.selected_host_alias() {
                    self.prompt_for_host_password(alias, Screen::Hosts, true);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                self.form = Some(HostForm::add());
                self.screen = Screen::HostForm;
                self.message = None;
            }
            KeyCode::Char('e') => self.start_edit_host(),
            KeyCode::Char('p') => self.start_password_prompt(Screen::Hosts, false),
            KeyCode::Char('d') if self.selected_host_alias().is_some() => {
                self.screen = Screen::DeleteHost;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.ssh_config.reload()?;
                self.clamp_host_selection();
                self.request_host_checks(tx);
                self.message = Some("ssh config reloaded".to_string());
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_files_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        if self
            .file
            .as_ref()
            .is_some_and(|file| file.delete_confirm.is_some())
        {
            self.handle_delete_confirm_key(key, tx);
            return Ok(());
        }

        if self
            .file
            .as_ref()
            .is_some_and(|file| file.confirm.is_some())
        {
            self.handle_confirm_key(key, tx);
            return Ok(());
        }

        if self
            .file
            .as_ref()
            .is_some_and(|file| file.transferring.is_some())
        {
            return Ok(());
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.direct_host {
                    self.should_quit = true;
                } else {
                    self.screen = Screen::Hosts;
                    self.file = None;
                    self.message = None;
                }
            }
            KeyCode::Tab => {
                if let Some(file) = self.file.as_mut() {
                    file.active = file.active.other();
                }
            }
            KeyCode::Left => {
                if let Some(file) = self.file.as_mut() {
                    file.active = Pane::Local;
                }
            }
            KeyCode::Right => {
                if let Some(file) = self.file.as_mut() {
                    file.active = Pane::Remote;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_file_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_file_selection(-1),
            KeyCode::PageDown => self.move_file_selection(10),
            KeyCode::PageUp => self.move_file_selection(-10),
            KeyCode::Enter => self.activate_file(tx),
            KeyCode::Backspace => self.go_parent(tx),
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_current_file_selection();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_all_files();
            }
            KeyCode::Char('.') => {
                if let Some(file) = self.file.as_mut() {
                    file.show_hidden = !file.show_hidden;
                }
                self.refresh_file_panes(tx)?;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.refresh_file_panes(tx)?;
            }
            KeyCode::Char('s') => {
                if let Some(file) = self.file.as_mut() {
                    file.sort = file.sort.next();
                    browser::sort_entries(&mut file.local_entries, file.sort);
                    browser::sort_entries(&mut file.remote_entries, file.sort);
                    file.local_state.select(Some(0));
                    file.remote_state.select(Some(0));
                }
            }
            KeyCode::Char('p') => {
                self.start_password_prompt(Screen::Files, false);
            }
            KeyCode::Char('g') => {
                self.start_path_prompt();
            }
            KeyCode::Char('f') => {
                self.start_search_prompt();
            }
            KeyCode::Char('n') => self.start_mkdir_prompt(),
            KeyCode::Char('e') => self.start_rename_prompt(),
            KeyCode::Char('x') | KeyCode::Char('D') => self.start_delete_confirm(),
            KeyCode::Char('b') => self.toggle_current_path_bookmark(),
            KeyCode::Char('B') | KeyCode::Char('o') => self.start_path_list(),
            _ => {}
        }

        Ok(())
    }

    fn handle_form_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(form) = self.form.as_mut() else {
            self.screen = Screen::Hosts;
            return Ok(());
        };

        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.screen = Screen::Hosts;
            }
            KeyCode::Tab | KeyCode::Down => form.field = form.field.next(),
            KeyCode::BackTab | KeyCode::Up => form.field = form.field.prev(),
            KeyCode::Enter => self.save_form(),
            KeyCode::Backspace => {
                form.value_mut().pop();
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                form.value_mut().push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_delete_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.screen = Screen::Hosts;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(alias) = self.selected_host_alias() {
                    match self.ssh_config.delete_host(&alias) {
                        Ok(()) => {
                            self.message = Some(format!("deleted Host {alias}"));
                            self.clamp_host_selection();
                        }
                        Err(err) => self.message = Some(err),
                    }
                }
                self.screen = Screen::Hosts;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_password_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        let Some(prompt) = self.password_prompt.as_mut() else {
            self.screen = Screen::Hosts;
            return Ok(());
        };

        match key.code {
            KeyCode::Esc => {
                let return_to = prompt.return_to;
                self.password_prompt = None;
                self.screen = return_to;
            }
            KeyCode::Enter => {
                let Some(prompt) = self.password_prompt.take() else {
                    return Ok(());
                };
                let host = prompt.host.clone();
                if prompt.input.is_empty() {
                    self.passwords.remove(&host);
                    self.set_open_file_password(&host, None);
                    self.message = Some(format!("cleared session password for {host}"));
                } else {
                    match PasswordHandle::new(&prompt.input) {
                        Ok(handle) => {
                            self.passwords.insert(host.clone(), handle.clone());
                            self.set_open_file_password(&host, Some(handle));
                            self.message = Some(format!("session password set for {host}"));
                        }
                        Err(err) => {
                            self.message = Some(format!("could not store password: {err}"));
                            self.screen = prompt.return_to;
                            return Ok(());
                        }
                    }
                }

                if prompt.open_after_save {
                    self.open_host(host)?;
                    self.start(tx);
                } else {
                    self.screen = prompt.return_to;
                    if let Some(file) = self.file.as_mut() {
                        file.request_remote_listing(file.remote_path.clone(), tx);
                    }
                }
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                prompt.input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_path_prompt_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        if self.path_prompt.is_none() {
            self.screen = Screen::Files;
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => {
                self.path_prompt = None;
                self.screen = Screen::Files;
                self.message = None;
            }
            KeyCode::Enter => {
                self.accept_path_completion();
                let Some((pane, input)) = self
                    .path_prompt
                    .as_ref()
                    .map(|prompt| (prompt.pane, prompt.input.clone()))
                else {
                    return Ok(());
                };
                if input.trim().is_empty() {
                    self.path_prompt = None;
                    self.screen = Screen::Files;
                    return Ok(());
                }

                match self.jump_to_path(pane, &input, tx) {
                    Ok(()) => {
                        self.path_prompt = None;
                        self.screen = Screen::Files;
                    }
                    Err(err) => self.message = Some(err),
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.path_prompt.as_mut() {
                    prompt.input.pop();
                    clear_prompt_completions(prompt);
                }
            }
            KeyCode::Up => self.move_path_completion(-1),
            KeyCode::Down => self.move_path_completion(1),
            KeyCode::PageUp => self.move_path_completion(-5),
            KeyCode::PageDown => self.move_path_completion(5),
            KeyCode::BackTab => self.move_path_completion(-1),
            KeyCode::Tab => {
                if self.accept_path_completion() {
                    return Ok(());
                }
                let Some((pane, input)) = self
                    .path_prompt
                    .as_ref()
                    .map(|prompt| (prompt.pane, prompt.input.clone()))
                else {
                    return Ok(());
                };
                match self.complete_path_input(pane, &input) {
                    Some(completed) => {
                        if let Some(prompt) = self.path_prompt.as_mut() {
                            if let Some(replacement) = completed.replacement {
                                prompt.input = replacement;
                            }
                            prompt.completions = completed.candidates;
                            prompt.completion_selected = 0;
                            prompt.completing = false;
                        }
                        self.message = None;
                    }
                    None => self.request_path_completion(pane, input, tx),
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(prompt) = self.path_prompt.as_mut() {
                    prompt.input.clear();
                    clear_prompt_completions(prompt);
                }
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(prompt) = self.path_prompt.as_mut() {
                    prompt.input.push(c);
                    clear_prompt_completions(prompt);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_path_completion(&mut self, delta: isize) {
        let Some(prompt) = self.path_prompt.as_mut() else {
            return;
        };
        let len = prompt.completions.len();
        if len == 0 {
            return;
        }

        let current = prompt.completion_selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        prompt.completion_selected = next;
    }

    fn accept_path_completion(&mut self) -> bool {
        let Some(prompt) = self.path_prompt.as_mut() else {
            return false;
        };
        let Some(candidate) = prompt
            .completions
            .get(prompt.completion_selected)
            .or_else(|| prompt.completions.first())
            .cloned()
        else {
            return false;
        };

        prompt.input = candidate;
        clear_prompt_completions(prompt);
        self.message = None;
        true
    }

    fn handle_search_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.search_prompt.is_none() {
            self.screen = Screen::Files;
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => {
                let Some((pane, previous)) = self
                    .search_prompt
                    .as_ref()
                    .map(|prompt| (prompt.pane, prompt.previous.clone()))
                else {
                    return Ok(());
                };
                if let Some(file) = self.file.as_mut() {
                    file.set_filter(pane, previous);
                }
                self.search_prompt = None;
                self.screen = Screen::Files;
                self.message = None;
            }
            KeyCode::Enter => {
                self.search_prompt = None;
                self.screen = Screen::Files;
                self.message = None;
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.search_prompt.as_mut() {
                    prompt.input.pop();
                }
                self.apply_search_prompt_filter();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(prompt) = self.search_prompt.as_mut() {
                    prompt.input.clear();
                }
                self.apply_search_prompt_filter();
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(prompt) = self.search_prompt.as_mut() {
                    prompt.input.push(c);
                }
                self.apply_search_prompt_filter();
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_path_list_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.screen = Screen::Files;
                self.message = None;
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_path_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_path_selection(-1),
            KeyCode::PageDown => self.move_path_selection(8),
            KeyCode::PageUp => self.move_path_selection(-8),
            KeyCode::Char('b') => self.toggle_selected_path_bookmark(),
            KeyCode::Enter => {
                let Some(entry) = self.selected_path_entry() else {
                    return Ok(());
                };
                match self.jump_to_path(entry.pane, &entry.path, tx) {
                    Ok(()) => {
                        self.screen = Screen::Files;
                    }
                    Err(err) => self.message = Some(err),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_name_prompt_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) -> Result<()> {
        if self.name_prompt.is_none() {
            self.screen = Screen::Files;
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => {
                self.name_prompt = None;
                self.screen = Screen::Files;
                self.message = None;
            }
            KeyCode::Enter => {
                let Some(prompt) = self.name_prompt.clone() else {
                    return Ok(());
                };
                let name = match validate_entry_name(&prompt.input) {
                    Ok(name) => name,
                    Err(err) => {
                        self.message = Some(err);
                        return Ok(());
                    }
                };

                let result = match prompt.kind {
                    NamePromptKind::Mkdir => self.start_mkdir(prompt.pane, name, tx),
                    NamePromptKind::Rename { old_name } => {
                        if old_name == name {
                            self.name_prompt = None;
                            self.screen = Screen::Files;
                            self.message = Some("name unchanged".to_string());
                            return Ok(());
                        }
                        self.start_rename(prompt.pane, old_name, name, tx)
                    }
                };

                match result {
                    Ok(()) => {
                        self.name_prompt = None;
                        self.screen = Screen::Files;
                    }
                    Err(err) => self.message = Some(err),
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.name_prompt.as_mut() {
                    prompt.input.pop();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(prompt) = self.name_prompt.as_mut() {
                    prompt.input.clear();
                }
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(prompt) = self.name_prompt.as_mut() {
                    prompt.input.push(c);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn save_form(&mut self) {
        let Some(form) = self.form.clone() else {
            self.screen = Screen::Hosts;
            return;
        };

        let result = match form.mode {
            HostFormMode::Add => self.ssh_config.add_host(&form.draft),
            HostFormMode::Edit => {
                let original = form.original_alias.as_deref().unwrap_or(&form.draft.alias);
                self.ssh_config.update_host(original, &form.draft)
            }
        };

        match result {
            Ok(()) => {
                self.message = Some(match form.mode {
                    HostFormMode::Add => format!("added Host {}", form.draft.alias),
                    HostFormMode::Edit => format!("saved Host {}", form.draft.alias),
                });
                self.form = None;
                self.screen = Screen::Hosts;
                self.select_host(&form.draft.alias);
            }
            Err(err) => self.message = Some(err),
        }
    }

    fn open_host(&mut self, host: String) -> Result<()> {
        let connection = Connection {
            password: self.passwords.get(&host).cloned(),
            host,
            config_path: Some(self.ssh_config.path.clone()),
        };
        let remote_home = remote::remote_pwd(&connection).unwrap_or_else(|_| ".".to_string());
        let remote_path = self
            .remote_start
            .clone()
            .unwrap_or_else(|| remote_home.clone());
        self.file = Some(FileSession::new(
            connection,
            self.local_start.clone(),
            remote_path,
            remote_home,
            self.show_hidden,
        )?);
        if let Some(file) = self.file.as_ref() {
            self.availability
                .entry(file.connection.host.clone())
                .or_insert(HostAvailability::Reachable { rtt_ms: 0 });
            self.history.record_open(&file.connection.host);
        }
        self.record_current_paths();
        self.screen = Screen::Files;
        self.message = None;
        Ok(())
    }

    fn start_password_prompt(&mut self, return_to: Screen, open_after_save: bool) {
        let host = match return_to {
            Screen::Hosts => self.selected_host_alias(),
            Screen::Files => self.file.as_ref().map(|file| file.connection.host.clone()),
            Screen::HostForm
            | Screen::DeleteHost
            | Screen::PasswordPrompt
            | Screen::PathPrompt
            | Screen::SearchPrompt
            | Screen::PathList
            | Screen::NamePrompt => None,
        };
        let Some(host) = host else {
            return;
        };
        self.prompt_for_host_password(host, return_to, open_after_save);
    }

    fn prompt_for_host_password(&mut self, host: String, return_to: Screen, open_after_save: bool) {
        self.password_prompt = Some(PasswordPrompt {
            host,
            input: String::new(),
            return_to,
            open_after_save,
        });
        self.screen = Screen::PasswordPrompt;
        self.message = None;
    }

    fn start_path_prompt(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let current = match file.active {
            Pane::Local => file.local_path.display().to_string(),
            Pane::Remote => file.remote_path.clone(),
        };
        self.path_prompt = Some(PathPrompt {
            pane: file.active,
            current,
            input: String::new(),
            completing: false,
            completions: Vec::new(),
            completion_selected: 0,
        });
        self.screen = Screen::PathPrompt;
        self.message = None;
    }

    fn start_search_prompt(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let input = file.filter_for(file.active).to_string();
        self.search_prompt = Some(SearchPrompt {
            pane: file.active,
            input: input.clone(),
            previous: input,
        });
        self.screen = Screen::SearchPrompt;
        self.message = None;
    }

    fn apply_search_prompt_filter(&mut self) {
        let Some((pane, input)) = self
            .search_prompt
            .as_ref()
            .map(|prompt| (prompt.pane, prompt.input.clone()))
        else {
            return;
        };
        if let Some(file) = self.file.as_mut() {
            file.set_filter(pane, input);
        }
    }

    fn jump_to_path(
        &mut self,
        pane: Pane,
        input: &str,
        tx: Sender<AppEvent>,
    ) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };

        match pane {
            Pane::Local => {
                let path = resolve_local_jump_path(&file.local_path, input)?;
                file.local_path = path;
                file.refresh_local()
                    .map_err(|err| format!("could not read local directory: {err}"))?;
                self.message = Some(format!("local path: {}", file.local_path.display()));
            }
            Pane::Remote => {
                let path = resolve_remote_jump_path(&file.remote_path, &file.remote_home, input)
                    .ok_or_else(|| "remote directory is required".to_string())?;
                file.request_remote_listing(path.clone(), tx);
                self.message = Some(format!("remote path: {path}"));
            }
        }
        self.record_path(pane);

        Ok(())
    }

    fn complete_path_input(&self, pane: Pane, input: &str) -> Option<PathCompletion> {
        let file = self.file.as_ref()?;
        match pane {
            Pane::Local => complete_local_path(&file.local_path, input),
            Pane::Remote => complete_remote_path(
                &file.remote_path,
                &file.remote_home,
                &file.remote_entries,
                input,
            ),
        }
    }

    fn request_path_completion(&mut self, pane: Pane, input: String, tx: Sender<AppEvent>) {
        let Some(file) = self.file.as_ref() else {
            return;
        };

        match pane {
            Pane::Local => {
                self.message = Some("no directory completion".to_string());
            }
            Pane::Remote => {
                let Some(query) =
                    remote_completion_query(&file.remote_path, &file.remote_home, &input)
                else {
                    self.message = Some("no directory completion".to_string());
                    return;
                };
                let connection = file.connection.clone();
                if let Some(prompt) = self.path_prompt.as_mut() {
                    prompt.completing = true;
                    prompt.completions.clear();
                    prompt.completion_selected = 0;
                }
                self.message = Some(format!("completing {}", query.base));
                thread::spawn(move || {
                    let result = remote::fetch_remote_directories(&connection, &query.base)
                        .map(|names| completion_from_names(&query.head, &query.prefix, &names));
                    let _ = tx.send(AppEvent::PathCompleted {
                        pane,
                        input,
                        result,
                    });
                });
            }
        }
    }

    fn set_open_file_password(&mut self, host: &str, password: Option<PasswordHandle>) {
        if let Some(file) = self.file.as_mut()
            && file.connection.host == host
        {
            file.connection.password = password;
        }
    }

    fn request_host_checks(&mut self, tx: Sender<AppEvent>) {
        let hosts = self
            .ssh_config
            .hosts()
            .iter()
            .map(|host| host.alias.clone())
            .collect::<Vec<_>>();
        for host in hosts {
            self.request_single_host_check(host, tx.clone());
        }
    }

    fn request_single_host_check(&mut self, host: String, tx: Sender<AppEvent>) {
        self.availability
            .insert(host.clone(), HostAvailability::Checking);
        let config_path = self.ssh_config.path.clone();
        thread::spawn(move || {
            let rtt_ms = remote::check_reachability(&host, Some(&config_path));
            let status = HostAvailability::from_rtt(rtt_ms);
            let _ = tx.send(AppEvent::HostChecked { host, status });
        });
    }

    fn start_edit_host(&mut self) {
        let Some(alias) = self.selected_host_alias() else {
            return;
        };
        let Some(host) = self
            .ssh_config
            .hosts()
            .iter()
            .find(|host| host.alias == alias)
        else {
            return;
        };
        if !host.editable() {
            self.message = Some("multi-alias or wildcard Host blocks are read-only".to_string());
            return;
        }
        self.form = Some(HostForm::edit(alias, host.to_draft()));
        self.screen = Screen::HostForm;
        self.message = None;
    }

    fn start_path_list(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let entries = self.path_store.entries(&file.connection.host, file.active);
        if entries.is_empty() {
            self.message = Some("no saved paths for this pane".to_string());
            return;
        }
        self.path_state.select(Some(0));
        self.screen = Screen::PathList;
        self.message = None;
    }

    fn toggle_current_path_bookmark(&mut self) {
        let Some((host, pane, path)) = self.current_path_context() else {
            return;
        };
        let pinned = self.path_store.toggle_bookmark(&host, pane, &path);
        self.message = Some(if pinned {
            format!("bookmarked {path}")
        } else {
            format!("unbookmarked {path}")
        });
    }

    fn toggle_selected_path_bookmark(&mut self) {
        let Some(entry) = self.selected_path_entry() else {
            return;
        };
        let pinned = self
            .path_store
            .toggle_bookmark(&entry.host, entry.pane, &entry.path);
        self.message = Some(if pinned {
            format!("bookmarked {}", entry.path)
        } else {
            format!("unbookmarked {}", entry.path)
        });
    }

    pub fn active_path_entries(&self) -> Vec<PathEntry> {
        let Some(file) = self.file.as_ref() else {
            return Vec::new();
        };
        self.path_store.entries(&file.connection.host, file.active)
    }

    fn selected_path_entry(&self) -> Option<PathEntry> {
        let entries = self.active_path_entries();
        let idx = self.path_state.selected().unwrap_or(0);
        entries.get(idx).cloned()
    }

    fn move_path_selection(&mut self, delta: isize) {
        let len = self.active_path_entries().len();
        if len == 0 {
            return;
        }
        let current = self.path_state.selected().unwrap_or(0).min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.path_state.select(Some(next));
    }

    fn record_current_paths(&mut self) {
        self.record_path(Pane::Local);
        self.record_path(Pane::Remote);
    }

    fn record_path(&mut self, pane: Pane) {
        let Some((host, pane, path)) = self.path_context(pane) else {
            return;
        };
        self.path_store.record_recent(&host, pane, &path);
    }

    fn current_path_context(&self) -> Option<(String, Pane, String)> {
        let file = self.file.as_ref()?;
        self.path_context(file.active)
    }

    fn path_context(&self, pane: Pane) -> Option<(String, Pane, String)> {
        let file = self.file.as_ref()?;
        let path = match pane {
            Pane::Local => file.local_path.display().to_string(),
            Pane::Remote => file.remote_path.clone(),
        };
        Some((file.connection.host.clone(), pane, path))
    }

    fn start_mkdir_prompt(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        if file.active == Pane::Remote && (file.remote_loading || file.remote_error.is_some()) {
            return;
        }
        self.name_prompt = Some(NamePrompt {
            pane: file.active,
            kind: NamePromptKind::Mkdir,
            input: String::new(),
        });
        self.screen = Screen::NamePrompt;
        self.message = None;
    }

    fn start_rename_prompt(&mut self) {
        let Some((pane, name)) = self.current_entry_name() else {
            return;
        };
        self.name_prompt = Some(NamePrompt {
            pane,
            kind: NamePromptKind::Rename {
                old_name: name.clone(),
            },
            input: name,
        });
        self.screen = Screen::NamePrompt;
        self.message = None;
    }

    fn start_delete_confirm(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.active == Pane::Remote && (file.remote_loading || file.remote_error.is_some()) {
            return;
        }
        let Some(plan) = delete_plan_from_selection_or_current(file) else {
            return;
        };
        file.delete_confirm = Some(plan);
        self.message = None;
    }

    fn start_mkdir(
        &mut self,
        pane: Pane,
        name: String,
        tx: Sender<AppEvent>,
    ) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        match pane {
            Pane::Local => {
                let target = file.local_path.join(&name);
                if target.exists() {
                    return Err(format!("target already exists: {name}"));
                }
                std::fs::create_dir_all(&target)
                    .map_err(|err| format!("could not create local directory: {err}"))?;
                self.refresh_file_panes(tx).map_err(|err| err.to_string())?;
                self.message = Some(format!("created local directory {name}"));
            }
            Pane::Remote => {
                let connection = file.connection.clone();
                let path = file.remote_path.clone();
                file.transferring = Some(format!("creating directory {name}"));
                thread::spawn(move || {
                    let result = remote::create_remote_dir(&connection, &path, &name)
                        .map(|()| format!("created remote directory {name}"));
                    let _ = tx.send(AppEvent::FileOperationFinished {
                        operation: FileOperation::Mkdir,
                        result,
                    });
                });
            }
        }
        Ok(())
    }

    fn start_rename(
        &mut self,
        pane: Pane,
        old_name: String,
        new_name: String,
        tx: Sender<AppEvent>,
    ) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        match pane {
            Pane::Local => {
                let source = file.local_path.join(&old_name);
                let target = file.local_path.join(&new_name);
                if target.exists() {
                    return Err(format!("target already exists: {new_name}"));
                }
                std::fs::rename(&source, &target)
                    .map_err(|err| format!("could not rename local entry: {err}"))?;
                self.refresh_file_panes(tx).map_err(|err| err.to_string())?;
                self.message = Some(format!("renamed {old_name} to {new_name}"));
            }
            Pane::Remote => {
                let connection = file.connection.clone();
                let path = file.remote_path.clone();
                file.transferring = Some(format!("renaming {old_name}"));
                thread::spawn(move || {
                    let result =
                        remote::rename_remote_entry(&connection, &path, &old_name, &new_name)
                            .map(|()| format!("renamed {old_name} to {new_name}"));
                    let _ = tx.send(AppEvent::FileOperationFinished {
                        operation: FileOperation::Rename,
                        result,
                    });
                });
            }
        }
        Ok(())
    }

    fn start_delete(&mut self, plan: DeletePlan, tx: Sender<AppEvent>) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        match plan.pane {
            Pane::Local => {
                delete_local_entries(&file.local_path, &plan.names)?;
                self.refresh_file_panes(tx).map_err(|err| err.to_string())?;
                self.message = Some(format!("deleted {} local item(s)", plan.names.len()));
            }
            Pane::Remote => {
                let connection = file.connection.clone();
                let path = file.remote_path.clone();
                let count = plan.names.len();
                let names = plan.names;
                file.transferring = Some(format!("deleting {count} item(s)"));
                thread::spawn(move || {
                    let result = remote::delete_remote_entries(&connection, &path, &names)
                        .map(|()| format!("deleted {count} remote item(s)"));
                    let _ = tx.send(AppEvent::FileOperationFinished {
                        operation: FileOperation::Delete,
                        result,
                    });
                });
            }
        }
        Ok(())
    }

    fn handle_confirm_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let plan = self.file.as_mut().and_then(|file| file.confirm.take());
                if let Some(plan) = plan {
                    self.start_transfer(plan, tx);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(file) = self.file.as_mut() {
                    file.confirm = None;
                }
            }
            _ => {}
        }
    }

    fn handle_delete_confirm_key(&mut self, key: KeyEvent, tx: Sender<AppEvent>) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let plan = self
                    .file
                    .as_mut()
                    .and_then(|file| file.delete_confirm.take());
                if let Some(plan) = plan
                    && let Err(err) = self.start_delete(plan, tx)
                {
                    self.message = Some(err);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(file) = self.file.as_mut() {
                    file.delete_confirm = None;
                }
            }
            _ => {}
        }
    }

    fn start_transfer(&mut self, plan: CopyPlan, tx: Sender<AppEvent>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let args = browser::build_scp_args(
            &file.connection.host,
            plan.source,
            &file.local_path,
            &file.remote_path,
            &plan.names,
            plan.has_dirs,
        );
        let direction = match plan.source {
            Pane::Local => "upload",
            Pane::Remote => "download",
        };
        file.transferring = Some(format!("{direction} {} item(s)", plan.names.len()));
        self.message = None;

        let connection = file.connection.clone();
        thread::spawn(move || {
            let result = remote::run_scp(&connection, &args);
            let _ = tx.send(AppEvent::TransferFinished { result });
        });
    }

    fn activate_file(&mut self, tx: Sender<AppEvent>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.active == Pane::Remote && (file.remote_loading || file.remote_error.is_some()) {
            return;
        }

        if let Some(plan) = copy_plan_from_selection(file) {
            file.confirm = Some(plan);
            return;
        }

        let idx = file.selected_index();
        if idx == 0 {
            self.go_parent(tx);
            return;
        }

        match file.active {
            Pane::Local => {
                let entries = browser::filter_entries(&file.local_entries, &file.local_filter);
                let Some(entry) = entries.get(idx - 1).cloned() else {
                    return;
                };
                if entry.is_dir {
                    file.local_path = file.local_path.join(entry.name);
                    let _ = file.refresh_local();
                    self.record_path(Pane::Local);
                } else {
                    file.confirm = Some(CopyPlan {
                        source: Pane::Local,
                        names: vec![entry.name],
                        has_dirs: false,
                    });
                }
            }
            Pane::Remote => {
                let entries = browser::filter_entries(&file.remote_entries, &file.remote_filter);
                let Some(entry) = entries.get(idx - 1).cloned() else {
                    return;
                };
                if entry.is_dir {
                    let new_path = browser::join_remote_path(&file.remote_path, &entry.name);
                    file.request_remote_listing(new_path, tx);
                    self.record_path(Pane::Remote);
                } else {
                    file.confirm = Some(CopyPlan {
                        source: Pane::Remote,
                        names: vec![entry.name],
                        has_dirs: false,
                    });
                }
            }
        }
    }

    fn go_parent(&mut self, tx: Sender<AppEvent>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        match file.active {
            Pane::Local => {
                if let Some(parent) = file.local_path.parent() {
                    file.local_path = parent.to_path_buf();
                    let _ = file.refresh_local();
                    self.record_path(Pane::Local);
                }
            }
            Pane::Remote => {
                if file.remote_loading {
                    return;
                }
                let parent = browser::parent_remote_path(&file.remote_path);
                if parent != file.remote_path {
                    file.request_remote_listing(parent, tx);
                    self.record_path(Pane::Remote);
                }
            }
        }
    }

    fn toggle_current_file_selection(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let idx = file.selected_index();
        if idx == 0 {
            return;
        }

        let name = match file.active {
            Pane::Local => browser::filter_entries(&file.local_entries, &file.local_filter)
                .get(idx - 1)
                .map(|entry| entry.name.clone()),
            Pane::Remote => browser::filter_entries(&file.remote_entries, &file.remote_filter)
                .get(idx - 1)
                .map(|entry| entry.name.clone()),
        };
        let Some(name) = name else {
            return;
        };

        let selected = match file.active {
            Pane::Local => &mut file.local_selected,
            Pane::Remote => &mut file.remote_selected,
        };
        if !selected.insert(name.clone()) {
            selected.remove(&name);
        }
    }

    fn toggle_all_files(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let (selected, entries) = match file.active {
            Pane::Local => (
                &mut file.local_selected,
                browser::filter_entries(&file.local_entries, &file.local_filter),
            ),
            Pane::Remote => (
                &mut file.remote_selected,
                browser::filter_entries(&file.remote_entries, &file.remote_filter),
            ),
        };

        if entries.iter().all(|entry| selected.contains(&entry.name)) && !entries.is_empty() {
            selected.clear();
        } else {
            selected.extend(entries.iter().map(|entry| entry.name.clone()));
        }
    }

    fn current_entry_name(&self) -> Option<(Pane, String)> {
        let file = self.file.as_ref()?;
        if file.active == Pane::Remote && (file.remote_loading || file.remote_error.is_some()) {
            return None;
        }
        let idx = file.selected_index();
        if idx == 0 {
            return None;
        }
        let name = match file.active {
            Pane::Local => browser::filter_entries(&file.local_entries, &file.local_filter)
                .get(idx - 1)
                .map(|entry| entry.name.clone()),
            Pane::Remote => browser::filter_entries(&file.remote_entries, &file.remote_filter)
                .get(idx - 1)
                .map(|entry| entry.name.clone()),
        }?;
        Some((file.active, name))
    }

    fn move_file_selection(&mut self, delta: isize) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let len = file.active_len();
        if len == 0 {
            return;
        }
        let current = file.active_state().selected().unwrap_or(0).min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        file.active_state().select(Some(next));
    }

    fn move_host_selection(&mut self, delta: isize) {
        let len = self.ssh_config.hosts().len();
        if len == 0 {
            return;
        }
        let current = self.host_state.selected().unwrap_or(0).min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.host_state.select(Some(next));
    }

    fn selected_host_alias(&self) -> Option<String> {
        let idx = self.host_state.selected().unwrap_or(0);
        self.ssh_config
            .hosts()
            .get(idx)
            .map(|host| host.alias.clone())
    }

    pub fn selected_host_stats(&self) -> HostStats {
        self.selected_host_alias()
            .map(|alias| self.history.stats(&alias))
            .unwrap_or_default()
    }

    fn select_host(&mut self, alias: &str) {
        if let Some(idx) = self
            .ssh_config
            .hosts()
            .iter()
            .position(|host| host.alias == alias)
        {
            self.host_state.select(Some(idx));
        }
    }

    fn clamp_host_selection(&mut self) {
        let len = self.ssh_config.hosts().len();
        if len == 0 {
            self.host_state.select(Some(0));
        } else {
            let idx = self.host_state.selected().unwrap_or(0).min(len - 1);
            self.host_state.select(Some(idx));
        }
    }
}

fn copy_plan_from_selection(file: &FileSession) -> Option<CopyPlan> {
    let (selected, entries) = match file.active {
        Pane::Local => (&file.local_selected, &file.local_entries),
        Pane::Remote => (&file.remote_selected, &file.remote_entries),
    };

    if selected.is_empty() {
        return None;
    }

    let mut names = selected.iter().cloned().collect::<Vec<_>>();
    names.sort();
    let has_dirs = names.iter().any(|name| {
        entries
            .iter()
            .any(|entry| entry.name == *name && entry.is_dir)
    });

    Some(CopyPlan {
        source: file.active,
        names,
        has_dirs,
    })
}

fn delete_plan_from_selection_or_current(file: &FileSession) -> Option<DeletePlan> {
    let (selected, all_entries, filter) = match file.active {
        Pane::Local => (
            &file.local_selected,
            &file.local_entries,
            &file.local_filter,
        ),
        Pane::Remote => (
            &file.remote_selected,
            &file.remote_entries,
            &file.remote_filter,
        ),
    };

    let names = if selected.is_empty() {
        let idx = file.selected_index();
        if idx == 0 {
            return None;
        }
        let entries = browser::filter_entries(all_entries, filter);
        vec![entries.get(idx - 1)?.name.clone()]
    } else {
        let mut names = selected.iter().cloned().collect::<Vec<_>>();
        names.sort();
        names
    };
    let has_dirs = names.iter().any(|name| {
        all_entries
            .iter()
            .any(|entry| entry.name == *name && entry.is_dir)
    });

    Some(DeletePlan {
        pane: file.active,
        names,
        has_dirs,
    })
}

fn delete_local_entries(base: &Path, names: &[String]) -> Result<(), String> {
    for name in names {
        let path = base.join(name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|err| format!("could not inspect {}: {err}", path.display()))?;
        if metadata.file_type().is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|err| format!("could not delete {}: {err}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .map_err(|err| format!("could not delete {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

fn validate_entry_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("name is required".to_string());
    }
    if name == "." || name == ".." {
        return Err("name cannot be . or ..".to_string());
    }
    if name.contains('/') || name.contains('\0') {
        return Err("name cannot contain /".to_string());
    }
    Ok(name.to_string())
}

fn resolve_local_jump_path(current: &Path, input: &str) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("local directory is required".to_string());
    }

    let path = expand_local_jump_path(current, raw)?;
    let path = path.canonicalize().unwrap_or(path);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!("not a local directory: {}", path.display()))
    }
}

fn expand_local_jump_path(current: &Path, input: &str) -> Result<PathBuf, String> {
    if input == "~" {
        return dirs::home_dir().ok_or_else(|| "could not determine home directory".to_string());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| "could not determine home directory".to_string());
    }

    let path = PathBuf::from(input);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(current.join(path))
    }
}

fn resolve_remote_jump_path(current: &str, home: &str, input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "~" {
        return Some(home.to_string());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Some(normalize_remote_path(&browser::join_remote_path(
            home, rest,
        )));
    }
    if raw.starts_with('/') {
        return Some(normalize_remote_path(raw));
    }
    Some(normalize_remote_path(&browser::join_remote_path(
        current, raw,
    )))
}

fn normalize_remote_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    if absolute {
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    } else if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn complete_local_path(current: &Path, input: &str) -> Option<PathCompletion> {
    let raw = input.trim();
    if raw == "~" {
        return Some(PathCompletion {
            replacement: Some("~/".to_string()),
            candidates: Vec::new(),
        });
    }

    let (head, prefix) = split_completion_input(raw);
    let dir = if head.is_empty() {
        current.to_path_buf()
    } else {
        let base = if head == "/" {
            "/"
        } else {
            head.trim_end_matches('/')
        };
        expand_local_jump_path(current, base).ok()?
    };
    let mut names = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(prefix).then_some(name)
        })
        .collect::<Vec<_>>();
    names.sort();
    completion_from_names(head, prefix, &names)
}

fn complete_remote_path(
    current: &str,
    home: &str,
    entries: &[Entry],
    input: &str,
) -> Option<PathCompletion> {
    let query = remote_completion_query(current, home, input)?;
    if normalize_remote_path(&query.base) != normalize_remote_path(current) {
        return None;
    }

    let mut names = entries
        .iter()
        .filter(|entry| entry.is_dir && entry.name.starts_with(&query.prefix))
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    completion_from_names(&query.head, &query.prefix, &names)
}

#[derive(Debug, Clone)]
struct PathCompletionQuery {
    base: String,
    head: String,
    prefix: String,
}

fn remote_completion_query(current: &str, home: &str, input: &str) -> Option<PathCompletionQuery> {
    let raw = input.trim();
    if raw == "~" {
        return Some(PathCompletionQuery {
            base: home.to_string(),
            head: "~/".to_string(),
            prefix: String::new(),
        });
    }

    let (head, prefix) = split_completion_input(raw);
    let base = if head.is_empty() {
        current.to_string()
    } else if head == "/" {
        "/".to_string()
    } else {
        resolve_remote_jump_path(current, home, head.trim_end_matches('/'))?
    };

    Some(PathCompletionQuery {
        base,
        head: head.to_string(),
        prefix: prefix.to_string(),
    })
}

fn split_completion_input(input: &str) -> (&str, &str) {
    input
        .rfind('/')
        .map(|idx| (&input[..=idx], &input[idx + 1..]))
        .unwrap_or(("", input))
}

fn completion_from_names(head: &str, prefix: &str, names: &[String]) -> Option<PathCompletion> {
    let matches = names
        .iter()
        .filter(|name| name.starts_with(prefix))
        .cloned()
        .collect::<Vec<_>>();
    let first = matches.first()?;
    let candidates = matches
        .iter()
        .take(8)
        .map(|name| format!("{head}{name}/"))
        .collect::<Vec<_>>();
    let replacement = if matches.len() == 1 {
        Some(format!("{head}{first}/"))
    } else {
        let completion = common_prefix(&matches);
        (completion.len() > prefix.len()).then_some(format!("{head}{completion}"))
    };

    Some(PathCompletion {
        replacement,
        candidates,
    })
}

fn clear_prompt_completions(prompt: &mut PathPrompt) {
    prompt.completing = false;
    prompt.completions.clear();
    prompt.completion_selected = 0;
}

fn common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for value in &values[1..] {
        while !value.starts_with(&prefix) {
            if prefix.pop().is_none() {
                return String::new();
            }
        }
    }
    prefix
}

fn is_auth_error(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("permission denied")
        || err.contains("authentication failed")
        || err.contains("read_passphrase")
        || err.contains("too many authentication failures")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_filters_unfiltered_names_by_prefix() {
        let names = ["archive", "storage", "storage-fast", "tmp"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();

        let completion = completion_from_names("", "stor", &names).expect("completion");

        assert_eq!(completion.replacement, Some("storage".to_string()));
        assert_eq!(
            completion.candidates,
            vec!["storage/".to_string(), "storage-fast/".to_string()]
        );
    }

    #[test]
    fn completion_returns_none_when_prefix_has_no_matches() {
        let names = ["archive", "tmp"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();

        assert!(completion_from_names("", "stor", &names).is_none());
    }

    #[test]
    fn entry_names_reject_path_separators() {
        assert_eq!(validate_entry_name("logs").unwrap(), "logs");
        assert!(validate_entry_name("").is_err());
        assert!(validate_entry_name("../logs").is_err());
        assert!(validate_entry_name(".").is_err());
    }
}
