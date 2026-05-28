use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, StatefulWidget, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, CopyPlan, DeletePlan, FileSession, HostAvailability, HostField, HostFormMode,
    NamePromptKind, Screen, TransferJob, TransferJobStatus,
};
use crate::browser::{self, Entry, Pane};
use crate::history::{HostStats, format_last_opened};
use crate::ssh_config::HostEntry;

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Hosts | Screen::DeleteHost => render_hosts(frame, app),
        Screen::Files => render_files(frame, app),
        Screen::HostForm => render_form(frame, app),
        Screen::PasswordPrompt => render_password(frame, app),
        Screen::PathPrompt => render_path_prompt(frame, app),
        Screen::SearchPrompt => render_search_prompt(frame, app),
        Screen::PathList => render_path_list(frame, app),
        Screen::NamePrompt => render_name_prompt(frame, app),
        Screen::TransferJobs => render_transfer_jobs(frame, app),
    }
}

fn render_hosts(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "sft",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  ssh config  "),
            Span::styled(
                app.ssh_config.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        rows[0],
    );

    let hosts = app.ssh_config.hosts().to_vec();
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(48), Constraint::Length(34)])
        .split(rows[1]);
    let availability = app.availability.clone();
    render_host_list(frame, body[0], &hosts, &availability, &mut app.host_state);
    let selected = app
        .host_state
        .selected()
        .and_then(|idx| hosts.get(idx))
        .cloned();
    let stats = app.selected_host_stats();
    render_host_sidebar(frame, body[1], selected.as_ref(), &stats, &availability);
    render_host_footer(frame, app, rows[2]);

    if app.screen == Screen::DeleteHost {
        render_delete_confirm(frame, app, area);
    }
}

fn render_host_list(
    frame: &mut Frame,
    area: Rect,
    hosts: &[HostEntry],
    availability: &std::collections::HashMap<String, HostAvailability>,
    state: &mut ratatui::widgets::ListState,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Hosts ({}) ", hosts.len()))
        .border_style(Style::default().fg(Color::Cyan));

    if hosts.is_empty() {
        let lines = vec![
            Line::from("No concrete Host entries found."),
            Line::from(""),
            Line::from(vec![
                key("n"),
                Span::raw(" add a host, or "),
                key("q"),
                Span::raw(" quit"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let width = area.width.saturating_sub(2) as usize;
    let items = hosts
        .iter()
        .map(|host| {
            let endpoint = format_endpoint(host);
            let readonly = if host.editable() { "" } else { " read-only" };
            let alias_width = 20usize.min(width / 3).max(10);
            let status_width = 4;
            let endpoint_width = width.saturating_sub(alias_width + status_width + 14);
            let status = *availability
                .get(&host.alias)
                .unwrap_or(&HostAvailability::Unknown);
            let (status_text, status_style) = availability_marker(status);
            ListItem::new(Line::from(vec![
                Span::styled(
                    pad_right(&truncate(&host.alias, alias_width), alias_width),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    pad_right(&truncate(&endpoint, endpoint_width), endpoint_width),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(pad_right(status_text, status_width), status_style),
                Span::styled(readonly.to_string(), Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(block)
        .highlight_symbol(">")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    StatefulWidget::render(list, area, frame.buffer_mut(), state);
}

fn render_host_sidebar(
    frame: &mut Frame,
    area: Rect,
    host: Option<&HostEntry>,
    stats: &HostStats,
    availability: &std::collections::HashMap<String, HostAvailability>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Details ")
        .border_style(Style::default().fg(Color::DarkGray));

    let Some(host) = host else {
        frame.render_widget(Paragraph::new("No host selected.").block(block), area);
        return;
    };

    let status = *availability
        .get(&host.alias)
        .unwrap_or(&HostAvailability::Unknown);
    let (marker, marker_style) = availability_marker(status);
    let status_label = availability_label(status);
    let chart_width = area.width.saturating_sub(4) as usize;

    let mut lines = vec![
        Line::from(vec![
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(status_label.clone(), marker_style),
        ]),
        Line::from(""),
        detail_line("Host", &host.alias),
        detail_line("HostName", host.hostname.as_deref().unwrap_or("-")),
        detail_line("User", host.user.as_deref().unwrap_or("-")),
        detail_line("Port", host.port.as_deref().unwrap_or("22")),
        detail_line("Identity", host.identity_file.as_deref().unwrap_or("-")),
        detail_line("Network", &status_label),
        Line::from(""),
        detail_line("Logins", &stats.opens.to_string()),
        detail_line("Last", &format_last_opened(stats.last_opened)),
    ];

    if chart_width >= 10 {
        let chart_lines = login_activity_chart(&stats.timestamps, chart_width, now_epoch());
        if !chart_lines.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Activity",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(chart_lines);
        }
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn render_host_footer(frame: &mut Frame, app: &App, area: Rect) {
    let lines = vec![
        Line::from(vec![
            key("Enter"),
            Span::raw(" password/open  "),
            key("n"),
            Span::raw(" new  "),
            key("e"),
            Span::raw(" edit  "),
            key("p"),
            Span::raw(" set password  "),
            key("d"),
            Span::raw(" delete  "),
            key("r"),
            Span::raw(" reload  "),
            key("q"),
            Span::raw(" quit"),
        ]),
        Line::from(Span::styled(
            app.message.clone().unwrap_or_default(),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_password(frame: &mut Frame, app: &mut App) {
    let return_to = app
        .password_prompt
        .as_ref()
        .map(|prompt| prompt.return_to)
        .unwrap_or(Screen::Hosts);

    match return_to {
        Screen::Files => render_files(frame, app),
        _ => render_hosts(frame, app),
    }

    let area = frame.area();
    let rect = centered_rect(area, 62, 8);
    frame.render_widget(Clear, rect);

    let Some(prompt) = app.password_prompt.as_ref() else {
        return;
    };
    let masked = "*".repeat(prompt.input.chars().count());
    let action = if prompt.open_after_save {
        "open"
    } else {
        "apply"
    };
    let empty_action = if prompt.open_after_save {
        "use key/agent"
    } else {
        "clear"
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("Host  "),
            Span::styled(prompt.host.clone(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Password")),
            Span::styled(masked, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Stored only for this sft process. Leave blank to use key/agent.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            key("Enter"),
            Span::raw(format!(" {action}  ")),
            key("Esc"),
            Span::raw(" cancel  "),
            key("empty Enter"),
            Span::raw(format!(" {empty_action}")),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Password ")),
        rect,
    );
}

fn render_path_prompt(frame: &mut Frame, app: &mut App) {
    render_files(frame, app);

    let area = frame.area();
    let Some(prompt) = app.path_prompt.as_ref() else {
        return;
    };
    let completion_count = prompt.completions.len().min(6) as u16;
    let rect = centered_rect(area, 74, 8 + completion_count);
    frame.render_widget(Clear, rect);

    let pane = prompt.pane.label();
    let value = if prompt.input.is_empty() {
        ""
    } else {
        &prompt.input
    };
    let completion_state = if prompt.completing {
        "completing..."
    } else {
        ""
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("Pane     "),
            Span::styled(pane, Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(completion_state, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::raw("Current  "),
            Span::styled(
                truncate(&prompt.current, rect.width.saturating_sub(13) as usize),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("Path     "),
            Span::styled(value.to_string(), Style::default().fg(Color::Cyan)),
        ]),
    ];

    if !prompt.completions.is_empty() {
        let total = prompt.completions.len();
        let selected = prompt.completion_selected.min(total.saturating_sub(1));
        let limit = 6usize;
        let start = if total <= limit {
            0
        } else {
            selected.saturating_sub(limit - 1).min(total - limit)
        };
        for (idx, candidate) in prompt
            .completions
            .iter()
            .enumerate()
            .skip(start)
            .take(limit)
        {
            let is_selected = idx == selected;
            let marker = if is_selected { ">" } else { " " };
            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(format!("{marker} "), style),
                Span::styled(
                    truncate(candidate, rect.width.saturating_sub(13) as usize),
                    style,
                ),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("Tab"),
        Span::raw(" complete  "),
        key("Up/Down"),
        Span::raw(" select  "),
        key("Enter"),
        Span::raw(" jump  "),
        key("Esc"),
        Span::raw(" cancel"),
    ]));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Go To ")),
        rect,
    );
}

fn render_search_prompt(frame: &mut Frame, app: &mut App) {
    render_files(frame, app);

    let area = frame.area();
    let rect = centered_rect(area, 64, 7);
    frame.render_widget(Clear, rect);

    let Some(prompt) = app.search_prompt.as_ref() else {
        return;
    };
    let count = app
        .file
        .as_ref()
        .map(|file| match prompt.pane {
            Pane::Local => browser::filter_entries(&file.local_entries, &prompt.input).len(),
            Pane::Remote => browser::filter_entries(&file.remote_entries, &prompt.input).len(),
        })
        .unwrap_or(0);
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("Pane    "),
            Span::styled(prompt.pane.label(), Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(
                format!("{count} matches"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("Find    "),
            Span::styled(prompt.input.clone(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![
            key("Enter"),
            Span::raw(" keep  "),
            key("Esc"),
            Span::raw(" cancel  "),
            key("^U"),
            Span::raw(" clear"),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Find ")),
        rect,
    );
}

fn render_path_list(frame: &mut Frame, app: &mut App) {
    render_files(frame, app);

    let area = frame.area();
    let Some(file) = app.file.as_ref() else {
        return;
    };
    let pane = file.active;
    let entries = app.active_path_entries();
    let rect = centered_rect(area, 76, 18);
    frame.render_widget(Clear, rect);

    let items = entries
        .iter()
        .map(|entry| {
            let mark = if entry.pinned { "*" } else { " " };
            let age = browser::format_relative_time(entry.last_used);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    truncate(&entry.path, rect.width.saturating_sub(20) as usize),
                    Style::default().fg(Color::White),
                ),
                Span::raw(" "),
                Span::styled(age, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} Paths ", pane.label())),
        )
        .highlight_symbol(">")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    StatefulWidget::render(list, rect, frame.buffer_mut(), &mut app.path_state);

    let help = Rect::new(
        rect.x + 2,
        rect.y + rect.height.saturating_sub(2),
        rect.width.saturating_sub(4),
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            key("Enter"),
            Span::raw(" jump  "),
            key("b"),
            Span::raw(" bookmark  "),
            key("Esc"),
            Span::raw(" close"),
        ])),
        help,
    );
}

fn render_name_prompt(frame: &mut Frame, app: &mut App) {
    render_files(frame, app);

    let area = frame.area();
    let rect = centered_rect(area, 62, 8);
    frame.render_widget(Clear, rect);

    let Some(prompt) = app.name_prompt.as_ref() else {
        return;
    };
    let (title, label, context) = match &prompt.kind {
        NamePromptKind::Mkdir => (" New Directory ", "Name", String::new()),
        NamePromptKind::Rename { old_name } => (" Rename ", "New name", old_name.clone()),
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("Pane     "),
            Span::styled(prompt.pane.label(), Style::default().fg(Color::Yellow)),
        ]),
    ];
    if !context.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("Current  "),
            Span::styled(
                truncate(&context, rect.width.saturating_sub(13) as usize),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::raw(format!("{label:<9}")),
        Span::styled(prompt.input.clone(), Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("Enter"),
        Span::raw(" apply  "),
        key("Esc"),
        Span::raw(" cancel"),
    ]));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        rect,
    );
}

fn render_transfer_jobs(frame: &mut Frame, app: &mut App) {
    render_files(frame, app);

    let area = frame.area();
    let rect = centered_rect(area, 82, 20);
    frame.render_widget(Clear, rect);

    let items = app
        .transfer_jobs
        .iter()
        .map(|job| {
            let (status, style) = transfer_status(job.status);
            let finished = job.finished_at.unwrap_or_else(now_epoch);
            let duration = finished.saturating_sub(job.started_at);
            let names = if job.names.len() == 1 {
                job.names[0].clone()
            } else {
                format!("{} items", job.names.len())
            };
            let detail = match job.status {
                TransferJobStatus::Failed => job.error.as_deref().unwrap_or("failed").to_string(),
                _ => format!(
                    "{} -> {}",
                    truncate(&job.local_path, 20),
                    truncate(&job.remote_path, 20)
                ),
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{:<7}", status), style),
                    Span::raw(" "),
                    Span::styled(job.direction.label(), Style::default().fg(Color::Yellow)),
                    Span::raw(format!("  {}  ", format_duration(duration))),
                    Span::styled(job.host.clone(), Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        truncate(&names, rect.width.saturating_sub(8) as usize),
                        Style::default().fg(Color::White),
                    ),
                ]),
                transfer_progress_line(job, rect.width.saturating_sub(8) as usize),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        truncate(&detail, rect.width.saturating_sub(8) as usize),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
            ])
        })
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Transfers "))
        .highlight_symbol(">")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    StatefulWidget::render(list, rect, frame.buffer_mut(), &mut app.transfer_state);

    let help = Rect::new(
        rect.x + 2,
        rect.y + rect.height.saturating_sub(2),
        rect.width.saturating_sub(4),
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            key("j/k"),
            Span::raw(" move  "),
            key("c"),
            Span::raw(" clear finished  "),
            key("t/Esc"),
            Span::raw(" close"),
        ])),
        help,
    );
}

fn render_form(frame: &mut Frame, app: &mut App) {
    render_hosts(frame, app);

    let area = frame.area();
    let rect = centered_rect(area, 72, 13);
    frame.render_widget(Clear, rect);

    let Some(form) = app.form.as_ref() else {
        return;
    };
    let title = match form.mode {
        HostFormMode::Add => " New Host ",
        HostFormMode::Edit => " Edit Host ",
    };

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    for field in HostField::ALL {
        let active = field == form.field;
        let marker = if active { ">" } else { " " };
        let style = if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(
                format!("{:<12}", field.label()),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(form.value(field).to_string(), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("Enter"),
        Span::raw(" save  "),
        key("Tab"),
        Span::raw(" next field  "),
        key("Esc"),
        Span::raw(" cancel"),
    ]));
    lines.push(Line::from(Span::styled(
        app.message.clone().unwrap_or_default(),
        Style::default().fg(Color::Yellow),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        rect,
    );
}

fn render_delete_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app
        .host_state
        .selected()
        .and_then(|idx| app.ssh_config.hosts().get(idx))
        .map(|host| host.alias.as_str())
        .unwrap_or("");

    let rect = centered_rect(area, 48, 7);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("Delete Host "),
                Span::styled(selected.to_string(), Style::default().fg(Color::Yellow)),
                Span::raw("?"),
            ]),
            Line::from(""),
            Line::from(vec![
                key("y/Enter"),
                Span::raw(" delete  "),
                key("n/Esc"),
                Span::raw(" cancel"),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
        rect,
    );
}

fn render_files(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let active_transfer_job = app.active_transfer_job().cloned();
    let Some(file) = app.file.as_mut() else {
        frame.render_widget(Paragraph::new("No host open."), area);
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

    render_file_header(frame, file, rows[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let local_entries = browser::filter_entries(&file.local_entries, &file.local_filter);
    let remote_entries = browser::filter_entries(&file.remote_entries, &file.remote_filter);
    let local_title = if file.local_filter.is_empty() {
        format!(" Local {} ", file.local_path.display())
    } else {
        format!(
            " Local {}  filter:{} ",
            file.local_path.display(),
            file.local_filter
        )
    };
    render_pane(
        frame,
        panes[0],
        &local_title,
        &local_entries,
        &file.local_selected,
        file.local_error.as_deref(),
        false,
        file.active == Pane::Local,
        &mut file.local_state,
    );

    let remote_title = if file.remote_filter.is_empty() {
        format!(" Remote {}:{} ", file.connection.host, file.remote_path)
    } else {
        format!(
            " Remote {}:{}  filter:{} ",
            file.connection.host, file.remote_path, file.remote_filter
        )
    };
    render_pane(
        frame,
        panes[1],
        &remote_title,
        &remote_entries,
        &file.remote_selected,
        file.remote_error.as_deref(),
        file.remote_loading,
        file.active == Pane::Remote,
        &mut file.remote_state,
    );

    render_file_footer(frame, app.message.as_deref(), file, rows[2]);

    if let Some(plan) = file.confirm.as_ref() {
        render_copy_confirm(frame, file, plan, area);
    }

    if let Some(plan) = file.delete_confirm.as_ref() {
        render_file_delete_confirm(frame, plan, area);
    }

    if let Some(label) = file.transferring.as_deref() {
        render_transfer(frame, label, active_transfer_job.as_ref(), area);
    }
}

fn render_file_header(frame: &mut Frame, file: &FileSession, area: Rect) {
    let active = format!("{} pane", file.active.label());
    let text = Line::from(vec![
        Span::styled(
            "sft",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}  ", file.connection.host)),
        Span::styled(active, Style::default().fg(Color::Yellow)),
        Span::raw(format!(
            "  sort:{}  hidden:{}",
            file.sort.label(),
            if file.show_hidden { "on" } else { "off" }
        )),
    ]);
    frame.render_widget(Paragraph::new(text), area);
}

#[allow(clippy::too_many_arguments)]
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[Entry],
    selected: &HashSet<String>,
    error: Option<&str>,
    loading: bool,
    active: bool,
    state: &mut ratatui::widgets::ListState,
) {
    let border_style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(truncate(title, area.width.saturating_sub(4) as usize))
        .border_style(border_style);

    if loading {
        let paragraph = Paragraph::new("Loading remote directory ...")
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    if let Some(error) = error {
        let paragraph = Paragraph::new(error.to_string())
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true })
            .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let items = file_items(entries, selected, area.width.saturating_sub(2) as usize);
    let list = List::new(items)
        .block(block)
        .highlight_symbol(">")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    StatefulWidget::render(list, area, frame.buffer_mut(), state);
}

fn file_items<'a>(
    entries: &[Entry],
    selected: &HashSet<String>,
    width: usize,
) -> Vec<ListItem<'a>> {
    let mut items = Vec::with_capacity(entries.len() + 1);
    items.push(ListItem::new(Line::from(Span::styled(
        "  ..",
        Style::default().fg(Color::DarkGray),
    ))));

    if entries.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  (empty)",
            Style::default().fg(Color::DarkGray),
        ))));
        return items;
    }

    let size_width = 10;
    let date_width = 7;
    let name_width = width.saturating_sub(size_width + date_width + 5);

    for entry in entries {
        let mark = if selected.contains(&entry.name) {
            "*"
        } else {
            " "
        };
        let name = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let name = truncate(&name, name_width);
        let name_padding = name_width.saturating_sub(name.width());
        let size = entry
            .size
            .map(browser::format_size)
            .unwrap_or_else(|| String::from(""));
        let modified = entry
            .modified
            .map(browser::format_relative_time)
            .unwrap_or_else(|| String::from(""));

        let name_style = if entry.is_dir {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else if selected.contains(&entry.name) {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {mark} "),
                if selected.contains(&entry.name) {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            Span::styled(name, name_style),
            Span::raw(" ".repeat(name_padding)),
            Span::styled(format!("{size:>9} "), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{modified:>6} "),
                Style::default().fg(Color::DarkGray),
            ),
        ])));
    }

    items
}

fn render_file_footer(frame: &mut Frame, message: Option<&str>, file: &FileSession, area: Rect) {
    let selected = match file.active {
        Pane::Local => file.local_selected.len(),
        Pane::Remote => file.remote_selected.len(),
    };
    let mut lines = vec![
        Line::from(vec![
            key("Enter"),
            Span::raw(" open/copy  "),
            key("Tab"),
            Span::raw(" switch  "),
            key("Space/m"),
            Span::raw(" select  "),
            key("a/^A"),
            Span::raw(" all  "),
            key("."),
            Span::raw(" hidden  "),
            key("s"),
            Span::raw(" sort  "),
            key("r"),
            Span::raw(" refresh  "),
        ]),
        Line::from(vec![
            key("g"),
            Span::raw(" goto  "),
            key("f"),
            Span::raw(" find  "),
            key("n"),
            Span::raw(" mkdir  "),
            key("e"),
            Span::raw(" rename  "),
            key("x"),
            Span::raw(" delete  "),
            key("b"),
            Span::raw(" mark  "),
            key("B/o"),
            Span::raw(" paths  "),
            key("t"),
            Span::raw(" jobs  "),
            key("p"),
            Span::raw(" password  "),
            key("q"),
            Span::raw(" back/quit"),
        ]),
    ];

    let status = message.unwrap_or(if selected > 0 { "selection active" } else { "" });
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} selected", selected),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(status.to_string(), Style::default().fg(Color::DarkGray)),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_copy_confirm(frame: &mut Frame, file: &FileSession, plan: &CopyPlan, area: Rect) {
    let direction = match plan.source {
        Pane::Local => "upload to remote",
        Pane::Remote => "download to local",
    };
    let destination = match plan.source {
        Pane::Local => format!("{}:{}", file.connection.host, file.remote_path),
        Pane::Remote => file.local_path.display().to_string(),
    };

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Copy {} item(s) - {direction}", plan.names.len()),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        destination,
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));
    for name in plan.names.iter().take(6) {
        lines.push(Line::from(format!("  {name}")));
    }
    if plan.names.len() > 6 {
        lines.push(Line::from(format!(
            "  ... and {} more",
            plan.names.len() - 6
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("y/Enter"),
        Span::raw(" copy  "),
        key("n/Esc"),
        Span::raw(" cancel"),
    ]));

    let rect = centered_rect(area, 62, (lines.len() + 2) as u16);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Confirm ")),
        rect,
    );
}

fn render_file_delete_confirm(frame: &mut Frame, plan: &DeletePlan, area: Rect) {
    let target = match plan.pane {
        Pane::Local => "local",
        Pane::Remote => "remote",
    };
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Delete {} {target} item(s)", plan.names.len()),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    if plan.has_dirs {
        lines.push(Line::from(Span::styled(
            "Includes directories; delete is recursive.",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    for name in plan.names.iter().take(6) {
        lines.push(Line::from(format!("  {name}")));
    }
    if plan.names.len() > 6 {
        lines.push(Line::from(format!(
            "  ... and {} more",
            plan.names.len() - 6
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("y/Enter"),
        Span::raw(" delete  "),
        key("n/Esc"),
        Span::raw(" cancel"),
    ]));

    let rect = centered_rect(area, 62, (lines.len() + 2) as u16);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Delete ")),
        rect,
    );
}

fn render_transfer(frame: &mut Frame, label: &str, job: Option<&TransferJob>, area: Rect) {
    let rect = centered_rect(area, 64, 7);
    frame.render_widget(Clear, rect);
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(label.to_string(), Style::default().fg(Color::Cyan)),
        ]),
    ];
    if let Some(job) = job {
        lines.push(transfer_progress_line(
            job,
            rect.width.saturating_sub(6) as usize,
        ));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("starting...", Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::from(""));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Transfer ")),
        rect,
    );
}

fn transfer_progress_line(job: &TransferJob, width: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            truncate(&transfer_progress_text(job), width),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn transfer_progress_text(job: &TransferJob) -> String {
    let speed = format_transfer_speed(job.speed_bps);
    let transferred = browser::format_size(job.transferred_bytes);
    match job.total_bytes {
        Some(total) => {
            let percent = transfer_percent(job).unwrap_or(0);
            let bar = transfer_progress_bar(job, 12);
            format!(
                "{bar} {percent:>3}%  {transferred} / {}  {speed}",
                browser::format_size(total)
            )
        }
        None => format!("{transferred} transferred  {speed}"),
    }
}

fn transfer_progress_bar(job: &TransferJob, width: usize) -> String {
    let Some(total) = job.total_bytes else {
        return String::new();
    };
    if width == 0 {
        return String::new();
    }
    let filled = if total == 0 {
        width
    } else {
        let transferred = job.transferred_bytes.min(total);
        ((transferred as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn transfer_percent(job: &TransferJob) -> Option<u64> {
    let total = job.total_bytes?;
    if total == 0 {
        return Some(100);
    }
    Some(((job.transferred_bytes.min(total) as u128 * 100) / total as u128) as u64)
}

fn format_transfer_speed(speed_bps: Option<u64>) -> String {
    speed_bps
        .map(|speed| format!("{}/s", browser::format_size(speed)))
        .unwrap_or_else(|| "--/s".to_string())
}

fn transfer_status(status: TransferJobStatus) -> (&'static str, Style) {
    match status {
        TransferJobStatus::Running => ("running", Style::default().fg(Color::Cyan)),
        TransferJobStatus::Succeeded => ("done", Style::default().fg(Color::Green)),
        TransferJobStatus::Failed => ("failed", Style::default().fg(Color::Red)),
    }
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn format_endpoint(host: &HostEntry) -> String {
    let user = host.user.as_deref().unwrap_or("");
    let hostname = host.hostname.as_deref().unwrap_or(&host.alias);
    let port = host.port.as_deref().unwrap_or("");
    let mut endpoint = String::new();
    if !user.is_empty() {
        endpoint.push_str(user);
        endpoint.push('@');
    }
    endpoint.push_str(hostname);
    if !port.is_empty() {
        endpoint.push(':');
        endpoint.push_str(port);
    }
    if let Some(identity) = host.identity_file.as_deref() {
        endpoint.push_str("  ");
        endpoint.push_str(identity);
    }
    endpoint
}

fn detail_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn availability_marker(status: HostAvailability) -> (&'static str, Style) {
    match status {
        HostAvailability::Unknown => ("○", Style::default().fg(Color::DarkGray)),
        HostAvailability::Checking => ("...", Style::default().fg(Color::Yellow)),
        HostAvailability::Reachable { .. } => ("●", Style::default().fg(Color::Green)),
        HostAvailability::Slow { .. } => ("▲", Style::default().fg(Color::Yellow)),
        HostAvailability::Unreachable => ("✖", Style::default().fg(Color::Red)),
    }
}

fn availability_label(status: HostAvailability) -> String {
    match status {
        HostAvailability::Unknown => "unknown".to_string(),
        HostAvailability::Checking => "checking".to_string(),
        HostAvailability::Reachable { rtt_ms } => format!("reachable ({})", format_rtt(rtt_ms)),
        HostAvailability::Slow { rtt_ms } => format!("slow ({})", format_rtt(rtt_ms)),
        HostAvailability::Unreachable => "unreachable".to_string(),
    }
}

fn format_rtt(ms: u32) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 9950 {
        format!("{:.1}s", ms as f32 / 1000.0)
    } else {
        "10s+".to_string()
    }
}

const ACTIVITY_BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const ACTIVITY_RANGES: &[(i64, &str, &str)] = &[
    (5, "5d", "~2d"),
    (10, "10d", "~5d"),
    (14, "2w", "~1w"),
    (30, "30d", "~2w"),
    (60, "2mo", "~1mo"),
    (180, "6mo", "~3mo"),
    (365, "1y", "~6mo"),
];

fn login_activity_chart(timestamps: &[i64], chart_width: usize, now: i64) -> Vec<Line<'static>> {
    if timestamps.is_empty() || chart_width == 0 {
        return Vec::new();
    }

    let oldest = timestamps
        .iter()
        .copied()
        .filter(|&ts| ts <= now)
        .min()
        .unwrap_or(now);
    let data_age_days = now.saturating_sub(oldest) / 86_400 + 1;
    let chart_days = ACTIVITY_RANGES
        .iter()
        .find(|(days, _, _)| *days >= data_age_days)
        .map(|(days, _, _)| *days)
        .or_else(|| ACTIVITY_RANGES.last().map(|range| range.0))
        .unwrap_or(365);
    let range_secs = chart_days * 86_400;
    let cutoff = now.saturating_sub(range_secs);
    let bucket_secs = range_secs as f64 / chart_width as f64;
    let mut buckets = vec![0u64; chart_width];

    for &ts in timestamps {
        if ts < cutoff || ts > now {
            continue;
        }
        let age = now.saturating_sub(ts);
        let idx =
            chart_width - 1 - ((age as f64 / bucket_secs).floor() as usize).min(chart_width - 1);
        buckets[idx] = buckets[idx].saturating_add(1);
    }

    if buckets.iter().all(|&bucket| bucket == 0) {
        return Vec::new();
    }

    let max_value = buckets.iter().copied().max().unwrap_or(1).max(1);
    let heights = buckets
        .iter()
        .map(|&bucket| {
            if bucket == 0 {
                0
            } else {
                ((bucket as f64 / max_value as f64) * 8.0).ceil() as usize
            }
        })
        .collect::<Vec<_>>();

    let mut spans = Vec::new();
    let mut empty_run = String::new();
    let mut filled_run = String::new();
    for height in heights {
        if height == 0 {
            if !filled_run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut filled_run),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            empty_run.push('·');
        } else {
            if !empty_run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut empty_run),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            filled_run.push(ACTIVITY_BLOCKS[height.min(8)]);
        }
    }
    if !filled_run.is_empty() {
        spans.push(Span::styled(
            filled_run,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !empty_run.is_empty() {
        spans.push(Span::styled(
            empty_run,
            Style::default().fg(Color::DarkGray),
        ));
    }

    vec![Line::from(spans), activity_axis(chart_width, chart_days)]
}

fn activity_axis(chart_width: usize, chart_days: i64) -> Line<'static> {
    let (left, middle) = ACTIVITY_RANGES
        .iter()
        .find(|(days, _, _)| *days == chart_days)
        .map(|(_, left, middle)| ((*left).to_string(), (*middle).to_string()))
        .unwrap_or_else(|| (format!("{chart_days}d"), String::new()));
    let right = "now";
    let label_width = left.len() + middle.len() + right.len();
    if !middle.is_empty() && chart_width > label_width + 4 {
        let total_gap = chart_width.saturating_sub(label_width);
        let left_gap = total_gap / 2;
        let right_gap = total_gap - left_gap;
        Line::from(vec![
            Span::styled(left, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(left_gap)),
            Span::styled(middle, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(right_gap)),
            Span::styled(right.to_string(), Style::default().fg(Color::DarkGray)),
        ])
    } else {
        let gap = chart_width.saturating_sub(left.len() + right.len());
        Line::from(vec![
            Span::styled(left, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(gap)),
            Span::styled(right.to_string(), Style::default().fg(Color::DarkGray)),
        ])
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn pad_right(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.width());
    format!("{value}{}", " ".repeat(padding))
}

fn truncate(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_string();
    }
    if max_width <= 1 {
        return "...".to_string();
    }

    let target = max_width.saturating_sub(3);
    let mut out = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push_str("...");
    out
}
