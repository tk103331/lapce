use std::{rc::Rc, sync::Arc};

use floem::views::editor::{core::register::Clipboard, text::SystemClipboard};
use floem::{
    AnyView, IntoView, View,
    action::{add_overlay, remove_overlay},
    event::EventListener,
    keyboard::Modifiers,
    kurbo::{Point, Size},
    reactive::{
        ReadSignal, RwSignal, SignalGet, SignalUpdate, create_effect, create_memo,
        create_rw_signal,
    },
    style::CursorStyle,
    views::{
        Decorators, container, dyn_container, dyn_stack, empty, h_stack, label,
        rich_text,
        scroll::{PropagatePointerWheel, scroll},
        stack, stack_from_iter,
    },
};
use lapce_core::{command::EditCommand, mode::Mode};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    agent::{
        AgentChoiceState, AgentConfigOption, AgentConfigOptionKind,
        AgentConfigValue, AgentContextUsage, AgentCost, AgentData, AgentMessage,
        AgentQuestionAnswer, AgentQuestionField, AgentQuestionKind,
        AgentQuestionValue, AgentRole, AgentSessionInfo, AgentStatus, AgentTask,
        AgentUsage, PendingPermission,
    },
    app::clickable_icon,
    command::{CommandExecuted, CommandKind},
    config::{LapceConfig, color::LapceColor, icon::LapceIcons},
    keypress::{KeyPressFocus, condition::Condition},
    markdown::{MarkdownContent, parse_markdown},
    text_input::TextInputBuilder,
    window_tab::{Focus, WindowTabData},
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectOption {
    id: String,
    label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectChrome {
    Bordered,
    Plain,
}

/// A compact select control. The list of options is rendered as an overlay
/// and opens upwards when there is not enough room below the control.
fn select_control(
    current_label: impl Fn() -> String + 'static,
    items: im::Vector<SelectOption>,
    on_accept: impl Fn(String) + 'static,
    window_size: RwSignal<Size>,
    config: ReadSignal<Arc<LapceConfig>>,
    chrome: SelectChrome,
    max_width: f32,
) -> impl View {
    let expanded = create_rw_signal(false);
    let window_origin = create_rw_signal(Point::ZERO);
    let control_size = create_rw_signal(Size::ZERO);
    let overlay_id = create_rw_signal(None);
    let list_size = create_rw_signal(Size::ZERO);
    let list_focus = create_rw_signal(false);
    let control_focus = create_rw_signal(false);

    let handler: Rc<dyn Fn(String)> = Rc::new(on_accept);
    let overlay_items = items.clone();
    let overlay_config = config;
    create_effect(move |_| {
        if expanded.get() {
            let items = overlay_items.clone();
            let handler = handler.clone();
            let id =
                add_overlay(Point::ZERO, move |_| {
                    let list_items = items.clone();
                    let list_handler = handler.clone();
                    scroll(
                        dyn_stack(
                            move || list_items.clone(),
                            |option| option.id.clone(),
                            move |option| {
                                let text = option.label.clone();
                                let id = option.id.clone();
                                let handler = list_handler.clone();
                                label(move || text.clone())
                                    .on_click_stop(move |_| {
                                        handler(id.clone());
                                        expanded.set(false);
                                    })
                                    .style(move |s| {
                                        let config = overlay_config.get();
                                        s.width_pct(100.0)
                                            .min_height(24.0)
                                            .items_center()
                                            .padding_horiz(10.0)
                                            .padding_vert(3.0)
                                            .text_ellipsis()
                                            .font_size(
                                                config.ui.font_size() as f32 - 1.0,
                                            )
                                            .color(config.color(
                                                LapceColor::EDITOR_FOREGROUND,
                                            ))
                                            .cursor(CursorStyle::Pointer)
                                            .selectable(false)
                                            .hover(|s| {
                                                s.background(config.color(
                                                LapceColor::PANEL_HOVERED_BACKGROUND,
                                            ))
                                            })
                                    })
                                    .into_any()
                            },
                        )
                        .style(|s| s.flex_col().width_pct(100.0)),
                    )
                    .style(move |s| {
                        let config = overlay_config.get();
                        let origin = window_origin.get();
                        let size = control_size.get();
                        let list = list_size.get();
                        let window = window_size.get();

                        let width = size.width.max(180.0);
                        let x = if origin.x + width + 6.0 > window.width {
                            (window.width - width - 6.0).max(0.0)
                        } else {
                            origin.x
                        };
                        let y = if origin.y + size.height + list.height + 6.0
                            > window.height
                        {
                            origin.y - list.height + 1.0
                        } else {
                            origin.y + size.height - 1.0
                        };

                        s.width(width)
                            .max_height(260.0)
                            .background(config.color(LapceColor::EDITOR_BACKGROUND))
                            .border(1.0)
                            .border_radius(6.0)
                            .border_color(config.color(LapceColor::LAPCE_BORDER))
                            .box_shadow_blur(6.0)
                            .box_shadow_color(
                                config.color(LapceColor::LAPCE_DROPDOWN_SHADOW),
                            )
                            .inset_left(x)
                            .inset_top(y)
                            .set(PropagatePointerWheel, false)
                    })
                    .on_resize(move |rect| list_size.set(rect.size()))
                    .keyboard_navigable()
                    .request_focus(|| {})
                    .on_event_stop(EventListener::FocusGained, move |_| {
                        list_focus.set(true);
                    })
                    .on_event_stop(EventListener::FocusLost, move |_| {
                        list_focus.set(false);
                        if !control_focus.get_untracked() {
                            expanded.set(false);
                        }
                    })
                    .on_event_stop(EventListener::PointerMove, |_| {})
                    .into_any()
                });
            overlay_id.set(Some(id));
        } else if let Some(id) = overlay_id.get_untracked() {
            remove_overlay(id);
            overlay_id.set(None);
        }
    });

    let control = stack((
        label(move || current_label()).style(move |s| {
            let config = config.get();
            s.min_width(0.0)
                .text_ellipsis()
                .font_size(if chrome == SelectChrome::Plain {
                    config.ui.font_size() as f32 + 2.0
                } else {
                    config.ui.font_size() as f32 - 1.0
                })
                .color(config.color(LapceColor::EDITOR_FOREGROUND))
                .selectable(false)
        }),
        empty().style(|s| s.flex_grow(1.0)),
        label(|| "▾".to_string()).style(move |s| {
            let config = config.get();
            s.font_size(config.ui.font_size() as f32 - 2.0)
                .color(config.color(LapceColor::EDITOR_DIM))
                .selectable(false)
        }),
    ))
    .on_click_stop(move |_| {
        expanded.update(|expanded| *expanded = !*expanded);
    })
    .on_move(move |point| {
        window_origin.set(point);
        if expanded.get_untracked() {
            expanded.set(false);
        }
    })
    .on_resize(move |rect| control_size.set(rect.size()))
    .style(move |s| {
        let config = config.get();
        s.flex_grow(1.0f32)
            .min_width(0.0)
            .max_width(max_width)
            .items_center()
            .gap(2.0)
            .cursor(CursorStyle::Pointer)
            .apply_if(chrome == SelectChrome::Bordered, |s| {
                s.padding_horiz(7.0)
                    .padding_vert(3.0)
                    .border(1.0)
                    .border_radius(5.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::EDITOR_BACKGROUND))
            })
            .apply_if(chrome == SelectChrome::Plain, |s| {
                s.padding_horiz(4.0).padding_vert(1.0)
            })
            .hover(|s| {
                s.background(config.color(LapceColor::PANEL_HOVERED_BACKGROUND))
            })
    })
    .keyboard_navigable()
    .on_event_stop(EventListener::FocusGained, move |_| {
        control_focus.set(true);
    })
    .on_event_stop(EventListener::FocusLost, move |_| {
        control_focus.set(false);
        if !list_focus.get_untracked() {
            expanded.set(false);
        }
    })
    .on_cleanup(move || {
        if let Some(id) = overlay_id.get_untracked() {
            remove_overlay(id);
        }
    });

    control
}

fn option_label(text: String, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    label(move || text.clone()).style(move |s| {
        let config = config.get();
        s.width(52.0)
            .min_width(0.0)
            .text_ellipsis()
            .font_size(config.ui.font_size() as f32 - 1.0)
            .color(config.color(LapceColor::EDITOR_DIM))
            .selectable(false)
    })
}

fn copy_text(text: String) {
    let mut clipboard = SystemClipboard::new();
    clipboard.put_string(text);
}

fn copy_icon(
    text: impl Fn() -> String + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    clickable_icon(
        || LapceIcons::COPY,
        move || copy_text(text()),
        || false,
        || false,
        || "Copy",
        config,
    )
}

fn preview_text(text: &str) -> String {
    let text = text.trim();
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut chars = first.chars();
    let mut preview: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_cost(cost: &AgentCost) -> String {
    if cost.currency == "USD" {
        format!("${:.4}", cost.amount)
    } else {
        format!("{:.4} {}", cost.amount, cost.currency)
    }
}

fn usage_summary(
    usage: Option<&AgentUsage>,
    context: Option<&AgentContextUsage>,
) -> String {
    let mut parts = Vec::new();
    if let Some(usage) = usage {
        if usage.total_tokens > 0 {
            parts.push(format!("{} tokens", format_token_count(usage.total_tokens)));
        }
    }
    if let Some(context) = context {
        if context.size > 0 {
            parts.push(format!(
                "ctx {}/{}",
                format_token_count(context.used),
                format_token_count(context.size)
            ));
        }
        if let Some(cost) = &context.cost {
            parts.push(format_cost(cost));
        }
    }
    parts.join(" · ")
}

pub fn agent_panel(
    window_tab_data: Rc<WindowTabData>,
    _position: PanelPosition,
) -> impl View {
    let agent = window_tab_data.agent.clone();
    let config = window_tab_data.common.config;
    let focus = window_tab_data.common.focus;
    let panel_focus = focus;

    stack((
        header(agent.clone(), config),
        session_history(agent.clone(), config),
        task_list(agent.clone(), config),
        conversation(agent.clone(), config),
        permission_view(agent.clone(), config),
        question_view(agent.clone(), config, focus),
        composer(agent, config, focus),
    ))
    .on_event_cont(EventListener::PointerDown, move |_| {
        panel_focus.set(Focus::Panel(PanelKind::Agent));
    })
    .style(|s| s.flex_col().size_full().min_height(0.0))
    .debug_name("Agent Panel")
}

fn new_session_selector(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    is_busy: impl Fn() -> bool + Copy + 'static,
) -> impl View {
    let window_size = agent.common.window_common.size;
    dyn_container(
        move || (agent.profiles.get(), is_busy()),
        move |(profiles, busy)| {
            if busy {
                return empty().into_any();
            }

            let items: im::Vector<SelectOption> = profiles
                .iter()
                .map(|profile| SelectOption {
                    id: profile.name.clone(),
                    label: profile.name.clone(),
                })
                .collect();
            if items.is_empty() {
                return empty().into_any();
            }

            let select_agent = agent.clone();
            select_control(
                || "+".to_string(),
                items,
                move |name| select_agent.new_session_with_profile(name),
                window_size,
                config,
                SelectChrome::Plain,
                44.0,
            )
            .into_any()
        },
    )
}

fn relative_time(created: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(created);
    let seconds = now.saturating_sub(created);
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn session_history(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    dyn_container(
        move || (agent.sessions_open.get(), agent.sessions.get()),
        move |(open, sessions)| {
            if !open {
                return empty().into_any();
            }

            let rows: Vec<AnyView> = sessions
                .into_iter()
                .map(|session| {
                    session_row(session, agent.clone(), config).into_any()
                })
                .collect();

            let body: AnyView = if rows.is_empty() {
                label(|| "No sessions yet".to_string())
                    .style(move |s| {
                        s.padding(10.0)
                            .font_size(config.get().ui.font_size() as f32 - 1.0)
                            .color(config.get().color(LapceColor::EDITOR_DIM))
                            .selectable(false)
                    })
                    .into_any()
            } else {
                scroll(stack_from_iter(rows).style(|s| s.flex_col().width_full()))
                    .scroll_style(|s| s.hide_bars(true))
                    .style(|s| s.width_full().max_height(220.0))
                    .into_any()
            };

            stack((
                label(|| "Sessions".to_string()).style(move |s| {
                    let config = config.get();
                    s.width_full()
                        .font_size(config.ui.font_size() as f32 - 1.0)
                        .color(config.color(LapceColor::EDITOR_DIM))
                        .selectable(false)
                }),
                body,
            ))
            .style(move |s| {
                let config = config.get();
                s.width_full()
                    .flex_col()
                    .gap(4.0)
                    .padding_horiz(10.0)
                    .padding_vert(6.0)
                    .border_bottom(1.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::PANEL_BACKGROUND))
            })
            .into_any()
        },
    )
}

fn session_row(
    session: AgentSessionInfo,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let title = session.title.clone();
    let meta = format!(
        "{} · {} messages · {}",
        if session.agent.is_empty() {
            "Agent"
        } else {
            session.agent.as_str()
        },
        session.messages,
        relative_time(session.created)
    );
    let active = session.active;
    let id = session.id;
    let switch_agent = agent.clone();

    h_stack((
        label(move || title.clone()).style(move |s| {
            let config = config.get();
            s.flex_grow(1.0f32)
                .min_width(0.0)
                .text_ellipsis()
                .font_size(config.ui.font_size() as f32)
                .color(config.color(LapceColor::EDITOR_FOREGROUND))
                .selectable(false)
        }),
        label(move || meta.clone()).style(move |s| {
            let config = config.get();
            s.font_size(config.ui.font_size() as f32 - 1.0)
                .color(config.color(LapceColor::EDITOR_DIM))
                .selectable(false)
        }),
    ))
    .on_click_stop(move |_| switch_agent.switch_session(id))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .items_center()
            .gap(8.0)
            .padding_horiz(6.0)
            .padding_vert(5.0)
            .border_radius(4.0)
            .cursor(CursorStyle::Pointer)
            .apply_if(active, |s| {
                s.background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
            })
            .hover(|s| {
                s.background(config.color(LapceColor::PANEL_HOVERED_BACKGROUND))
            })
    })
}

fn header(agent: AgentData, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    let is_busy = {
        let agent = agent.clone();
        create_memo(move |_| agent.status.get().is_busy())
    };

    let name_agent = agent.clone();
    let status_label_agent = agent.clone();
    let status_style_agent = agent.clone();
    let usage_agent = agent.clone();
    let usage_text = create_memo(move |_| {
        usage_summary(
            usage_agent.usage.get().as_ref(),
            usage_agent.context_usage.get().as_ref(),
        )
    });
    let new_session = agent.clone();
    let sessions_button = agent.clone();
    let sessions_active = agent.clone();
    let stop = agent.clone();
    let restart = agent.clone();

    h_stack((
        stack((
            label(move || {
                name_agent
                    .agent_name
                    .get()
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        let active = name_agent.active_profile.get();
                        (!active.is_empty()).then_some(active)
                    })
                    .unwrap_or_else(|| "Agent".to_string())
            })
            .style(move |s| {
                s.font_size(config.get().ui.font_size() as f32)
                    .color(config.get().color(LapceColor::EDITOR_FOREGROUND))
                    .text_ellipsis()
                    .min_width(0.0)
                    .selectable(false)
            }),
            label(move || status_label_agent.status.get().label()).style(move |s| {
                let config = config.get();
                let color = match status_style_agent.status.get() {
                    AgentStatus::Failed(_) => config.color(LapceColor::LAPCE_ERROR),
                    AgentStatus::WaitingForPermission
                    | AgentStatus::WaitingForAnswer => {
                        config.color(LapceColor::LAPCE_WARN)
                    }
                    _ => config.color(LapceColor::EDITOR_DIM),
                };
                s.font_size(config.ui.font_size() as f32 - 1.0)
                    .color(color)
                    .text_ellipsis()
                    .min_width(0.0)
                    .selectable(false)
            }),
            label(move || usage_text.get()).style(move |s| {
                let config = config.get();
                s.font_size(config.ui.font_size() as f32 - 1.0)
                    .color(config.color(LapceColor::EDITOR_DIM))
                    .text_ellipsis()
                    .min_width(0.0)
                    .selectable(false)
                    .apply_if(usage_text.get().is_empty(), |s| s.hide())
            }),
        ))
        .style(|s| s.flex_col().flex_grow(1.0f32).min_width(0.0)),
        h_stack((
            clickable_icon(
                || LapceIcons::HISTORY,
                move || sessions_button.toggle_sessions(),
                move || sessions_active.sessions_open.get(),
                || false,
                || "Session history",
                config,
            ),
            new_session_selector(new_session.clone(), config, move || is_busy.get()),
            clickable_icon(
                || LapceIcons::DEBUG_STOP,
                move || stop.cancel(),
                || false,
                move || !is_busy.get(),
                || "Stop",
                config,
            ),
            clickable_icon(
                || LapceIcons::DEBUG_RESTART,
                move || restart.restart(),
                || false,
                || false,
                || "Restart agent",
                config,
            ),
        ))
        .style(|s| s.items_center()),
    ))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .items_center()
            .padding_horiz(10.0)
            .padding_vert(6.0)
            .border_bottom(1.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::EDITOR_BACKGROUND))
    })
    .debug_name("Agent Header")
}

fn conversation(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let content_height = create_rw_signal(0.0);

    let empty_state = {
        let agent = agent.clone();
        dyn_container(
            move || agent.messages.get().is_empty(),
            move |is_empty| {
                if !is_empty {
                    return empty().into_any();
                }

                stack((
                    label(|| "Agent Chat".to_string()).style(move |s| {
                        s.font_size(config.get().ui.font_size() as f32 + 2.0)
                            .color(config.get().color(LapceColor::EDITOR_FOREGROUND))
                            .selectable(false)
                    }),
                    label(|| {
                        "Send a message to start the agent. The conversation \
                         is powered by the Agent Client Protocol."
                            .to_string()
                    })
                    .style(move |s| {
                        s.font_size(config.get().ui.font_size() as f32)
                            .color(config.get().color(LapceColor::EDITOR_DIM))
                            .selectable(false)
                    }),
                    label(move || {
                        format!("Command: {}", config.get().agent.command)
                    })
                    .style(move |s| {
                        s.font_size(config.get().ui.font_size() as f32 - 1.0)
                            .color(config.get().color(LapceColor::EDITOR_DIM))
                            .text_ellipsis()
                            .min_width(0.0)
                            .selectable(false)
                    }),
                ))
                .style(|s| s.flex_col().width_full().padding(14.0).gap(6.0))
                .into_any()
            },
        )
    };

    let items_agent = agent.clone();
    let messages_agent = agent.clone();
    let scroll_agent = agent.clone();
    let scroll_to_agent = agent.clone();

    scroll(
        stack((
            empty_state,
            dyn_stack(
                move || items_agent.messages.get(),
                |message| message.get_untracked().id,
                move |message| message_view(message, messages_agent.clone(), config),
            )
            .style(|s| s.flex_col().width_full()),
        ))
        .on_resize(move |rect| content_height.set(rect.height()))
        .style(|s| s.flex_col().width_full().min_width(0.0)),
    )
    .on_scroll(move |rect| {
        let at_bottom = rect.y1 >= content_height.get_untracked() - 32.0;
        scroll_agent.auto_scroll.set(at_bottom);
    })
    .scroll_to(move || {
        if !scroll_to_agent.auto_scroll.get() {
            return None;
        }
        // Track incoming updates so that we stay pinned to the bottom while
        // the agent is streaming a response.
        let _ = scroll_to_agent.revision.get();
        Some(Point::new(0.0, content_height.get() + 1000.0))
    })
    .style(|s| {
        s.flex_grow(1.0f32)
            .flex_basis(0.0)
            .min_height(0.0)
            .width_full()
    })
    .debug_name("Agent Conversation")
}

fn task_list(agent: AgentData, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    dyn_container(
        move || agent.tasks.get(),
        move |tasks| {
            if tasks.is_empty() {
                return empty().into_any();
            }

            let total = tasks.len();
            let completed = tasks
                .iter()
                .filter(|task| task.status == "completed")
                .count();
            let title = format!("Tasks ({completed}/{total})");

            stack((
                label(move || title.clone()).style(move |s| {
                    let config = config.get();
                    s.width_full()
                        .font_size(config.ui.font_size() as f32 - 1.0)
                        .color(config.color(LapceColor::EDITOR_FOREGROUND))
                        .selectable(false)
                }),
                scroll(
                    stack_from_iter(
                        tasks.into_iter().map(|task| task_row(task, config)),
                    )
                    .style(|s| s.flex_col().width_full().gap(3.0)),
                )
                .scroll_style(|s| s.hide_bars(true))
                .style(|s| s.width_full().max_height(140.0)),
            ))
            .style(move |s| {
                let config = config.get();
                s.width_full()
                    .flex_col()
                    .gap(4.0)
                    .padding_horiz(10.0)
                    .padding_vert(6.0)
                    .border_bottom(1.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::PANEL_BACKGROUND))
            })
            .into_any()
        },
    )
}

fn task_row(task: AgentTask, config: ReadSignal<Arc<LapceConfig>>) -> AnyView {
    let (icon, color) = match task.status.as_str() {
        "completed" => ("✓", LapceColor::EDITOR_DIM),
        "in_progress" => ("→", LapceColor::LAPCE_WARN),
        _ => ("○", LapceColor::EDITOR_DIM),
    };
    let completed = task.status == "completed";

    h_stack((
        label(move || icon)
            .style(move |s| s.color(config.get().color(color)).selectable(false)),
        label(move || task.content.clone()).style(move |s| {
            let config = config.get();
            s.flex_grow(1.0f32)
                .min_width(0.0)
                .text_ellipsis()
                .font_size(config.ui.font_size() as f32 - 1.0)
                .color(if completed {
                    config.color(LapceColor::EDITOR_DIM)
                } else {
                    config.color(LapceColor::EDITOR_FOREGROUND)
                })
                .selectable(false)
        }),
    ))
    .style(|s| s.width_full().items_center().gap(6.0))
    .into_any()
}

fn question_view(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    focus: RwSignal<Focus>,
) -> impl View {
    dyn_container(
        move || (agent.question.get(), agent.question_values.get()),
        move |(question, values)| {
            let Some(question) = question else {
                return empty().into_any();
            };

            let submit_agent = agent.clone();
            let decline_agent = agent.clone();
            let cancel_agent = agent.clone();
            let fields: Vec<AnyView> = question
                .fields
                .iter()
                .map(|field| {
                    question_field(field, &values, agent.clone(), config, focus)
                })
                .collect();

            stack((
                label(|| "User input required".to_string()).style(move |s| {
                    let config = config.get();
                    s.width_full()
                        .font_size(config.ui.font_size() as f32 - 1.0)
                        .color(config.color(LapceColor::LAPCE_WARN))
                        .selectable(false)
                }),
                markdown_text(question.message, config),
                scroll(
                    stack_from_iter(fields)
                        .style(|s| s.flex_col().width_full().gap(8.0)),
                )
                .scroll_style(|s| s.hide_bars(true))
                .style(|s| s.width_full().max_height(220.0)),
                h_stack((
                    action_button(
                        "Submit".to_string(),
                        move || submit_agent.submit_question_answer(),
                        config,
                        true,
                    ),
                    action_button(
                        "Decline".to_string(),
                        move || {
                            decline_agent
                                .answer_question(AgentQuestionAnswer::Decline)
                        },
                        config,
                        false,
                    ),
                    action_button(
                        "Cancel".to_string(),
                        move || {
                            cancel_agent.answer_question(AgentQuestionAnswer::Cancel)
                        },
                        config,
                        false,
                    ),
                ))
                .style(|s| s.gap(6.0)),
            ))
            .style(move |s| {
                let config = config.get();
                s.width_full()
                    .flex_col()
                    .gap(6.0)
                    .padding(10.0)
                    .border_top(1.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::PANEL_BACKGROUND))
            })
            .into_any()
        },
    )
}

fn question_field(
    field: &AgentQuestionField,
    values: &im::HashMap<String, AgentQuestionValue>,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    focus: RwSignal<Focus>,
) -> AnyView {
    let name = field.name.clone();
    let title = if field.required {
        format!("{} *", field.title)
    } else {
        field.title.clone()
    };

    let control: AnyView = match &field.kind {
        AgentQuestionKind::Text { .. }
        | AgentQuestionKind::Integer { .. }
        | AgentQuestionKind::Number { .. } => {
            let Some(editor) = agent.question_input(&field.name) else {
                return empty().into_any();
            };
            let placeholder = match &field.kind {
                AgentQuestionKind::Integer { .. } => "Integer",
                AgentQuestionKind::Number { .. } => "Number",
                _ => "Type an answer",
            };
            let input = TextInputBuilder::new()
                .is_focused(move || focus.get() == Focus::Panel(PanelKind::Agent))
                .build_editor(editor)
                .placeholder(move || placeholder.to_string())
                .keyboard_navigable();
            let input_id = input.id();
            container(input.on_event_cont(EventListener::PointerDown, move |_| {
                focus.set(Focus::Panel(PanelKind::Agent));
                input_id.request_focus();
            }))
            .style(move |s| {
                let config = config.get();
                s.width_full()
                    .min_width(0.0)
                    .border(1.0)
                    .border_radius(4.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::EDITOR_BACKGROUND))
            })
            .into_any()
        }
        AgentQuestionKind::Boolean { .. } => {
            let current = matches!(
                values.get(&field.name),
                Some(AgentQuestionValue::Boolean(true))
            );
            let name = name.clone();
            let toggle_agent = agent.clone();
            action_button(
                if current { "On" } else { "Off" }.to_string(),
                move || toggle_agent.toggle_question_bool(&name),
                config,
                current,
            )
            .into_any()
        }
        AgentQuestionKind::Select { options, .. } => {
            let items: im::Vector<SelectOption> = options
                .iter()
                .map(|option| SelectOption {
                    id: option.value.clone(),
                    label: option.title.clone(),
                })
                .collect();
            if items.is_empty() {
                return empty().into_any();
            }
            let current_value = match values.get(&field.name) {
                Some(AgentQuestionValue::Select(value)) => value.clone(),
                _ => items.get(0).map(|item| item.id.clone()).unwrap_or_default(),
            };
            let current_label = items
                .iter()
                .find(|choice| choice.id == current_value)
                .or_else(|| items.get(0))
                .map(|choice| choice.label.clone())
                .unwrap_or_default();
            let name = name.clone();
            let select_agent = agent.clone();
            let window_size = agent.common.window_common.size;
            select_control(
                move || current_label.clone(),
                items,
                move |value| select_agent.set_question_select(&name, value),
                window_size,
                config,
                SelectChrome::Bordered,
                f32::MAX,
            )
            .into_any()
        }
        AgentQuestionKind::MultiSelect { options, .. } => {
            let buttons: Vec<AnyView> = options
                .iter()
                .map(|option| {
                    let selected = matches!(
                        values.get(&field.name),
                        Some(AgentQuestionValue::Multi(values))
                            if values.contains(&option.value)
                    );
                    let name = name.clone();
                    let value = option.value.clone();
                    let toggle_agent = agent.clone();
                    action_button(
                        option.title.clone(),
                        move || {
                            toggle_agent.toggle_question_multi(&name, value.clone())
                        },
                        config,
                        selected,
                    )
                    .into_any()
                })
                .collect();
            stack_from_iter(buttons)
                .style(|s| s.flex_col().width_full().gap(3.0))
                .into_any()
        }
    };

    stack((
        label(move || title.clone()).style(move |s| {
            let config = config.get();
            s.width_full()
                .min_width(0.0)
                .text_ellipsis()
                .font_size(config.ui.font_size() as f32 - 1.0)
                .color(config.color(LapceColor::EDITOR_FOREGROUND))
                .selectable(false)
        }),
        field
            .description
            .clone()
            .map(|description| {
                label(move || description.clone())
                    .style(move |s| {
                        let config = config.get();
                        s.width_full()
                            .font_size(config.ui.font_size() as f32 - 1.0)
                            .color(config.color(LapceColor::EDITOR_DIM))
                            .selectable(false)
                    })
                    .into_any()
            })
            .unwrap_or_else(|| empty().into_any()),
        control,
    ))
    .style(|s| s.width_full().flex_col().gap(3.0))
    .into_any()
}

fn config_bar(agent: AgentData, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    options_panel(agent, config)
}

fn options_panel(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    dyn_container(
        move || {
            (
                agent.models.get(),
                agent.modes.get(),
                agent.config_options.get(),
            )
        },
        move |(models, modes, config_options)| {
            let mut rows: Vec<AnyView> = Vec::new();
            let window_size = agent.common.window_common.size;

            let model = models.and_then(|models| {
                model_row(models, agent.clone(), config, window_size)
            });
            let mode = modes.and_then(|modes| {
                mode_row(modes, agent.clone(), config, window_size)
            });
            match (model, mode) {
                (Some(model), Some(mode)) => {
                    rows.push(
                        h_stack((model, mode))
                            .style(|s| s.width_full().items_center().gap(10.0))
                            .into_any(),
                    );
                }
                (Some(row), None) | (None, Some(row)) => rows.push(row),
                (None, None) => {}
            }
            for option in config_options {
                if let Some(row) = config_option_row(option, agent.clone(), config) {
                    rows.push(row);
                }
            }

            if rows.is_empty() {
                return empty().into_any();
            }

            scroll(
                stack_from_iter(rows).style(|s| s.flex_col().width_full().gap(3.0)),
            )
            .scroll_style(|s| s.hide_bars(true))
            .style(|s| s.width_full().max_height(150.0))
            .into_any()
        },
    )
}

fn model_row(
    state: AgentChoiceState,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    window_size: RwSignal<Size>,
) -> Option<AnyView> {
    let items: im::Vector<SelectOption> = state
        .available
        .into_iter()
        .map(|model| SelectOption {
            id: model.id,
            label: model.name,
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    let name = items
        .iter()
        .find(|choice| choice.id == state.current)
        .or_else(|| items.get(0))
        .map(|choice| choice.label.clone())?;

    let dropdown = select_control(
        move || name.clone(),
        items,
        move |id| agent.select_model(id),
        window_size,
        config,
        SelectChrome::Bordered,
        f32::MAX,
    );

    Some(
        h_stack((option_label("Model".to_string(), config), dropdown))
            .style(|s| {
                s.flex_grow(1.0f32)
                    .flex_basis(0.0)
                    .min_width(0.0)
                    .items_center()
                    .gap(6.0)
            })
            .into_any(),
    )
}

fn mode_row(
    state: AgentChoiceState,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    window_size: RwSignal<Size>,
) -> Option<AnyView> {
    let items: im::Vector<SelectOption> = state
        .available
        .into_iter()
        .map(|mode| SelectOption {
            id: mode.id,
            label: mode.name,
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    let name = items
        .iter()
        .find(|choice| choice.id == state.current)
        .or_else(|| items.get(0))
        .map(|choice| choice.label.clone())?;

    let dropdown = select_control(
        move || name.clone(),
        items,
        move |id| agent.select_mode(id),
        window_size,
        config,
        SelectChrome::Bordered,
        f32::MAX,
    );

    Some(
        h_stack((option_label("Mode".to_string(), config), dropdown))
            .style(|s| {
                s.flex_grow(1.0f32)
                    .flex_basis(0.0)
                    .min_width(0.0)
                    .items_center()
                    .gap(6.0)
            })
            .into_any(),
    )
}

fn config_option_row(
    option: AgentConfigOption,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> Option<AnyView> {
    let AgentConfigOption { id, name, kind, .. } = option;

    match kind {
        AgentConfigOptionKind::Select { current, options } => {
            let items: Vec<SelectOption> = options
                .into_iter()
                .map(|option| SelectOption {
                    id: option.value,
                    label: option.name,
                })
                .collect();
            if items.is_empty() {
                return None;
            }
            let current_label = items
                .iter()
                .find(|choice| choice.id == current)
                .or_else(|| items.get(0))
                .map(|choice| choice.label.clone())?;
            let options: im::Vector<SelectOption> = items.into_iter().collect();
            let config_id = id;
            let window_size = agent.common.window_common.size;
            let dropdown = select_control(
                move || current_label.clone(),
                options,
                move |value| {
                    agent.set_config_option(
                        &config_id,
                        AgentConfigValue::Select(value),
                    )
                },
                window_size,
                config,
                SelectChrome::Bordered,
                f32::MAX,
            );

            Some(
                h_stack((option_label(name, config), dropdown))
                    .style(|s| s.width_full().items_center().gap(6.0))
                    .into_any(),
            )
        }
        AgentConfigOptionKind::Boolean { current } => {
            let config_id = id;
            let label = if current { "On" } else { "Off" };

            Some(
                h_stack((
                    option_label(name, config),
                    action_button(
                        label.to_string(),
                        move || {
                            agent.set_config_option(
                                &config_id,
                                AgentConfigValue::Boolean(!current),
                            )
                        },
                        config,
                        current,
                    ),
                ))
                .style(|s| s.width_full().items_center().gap(6.0))
                .into_any(),
            )
        }
    }
}

fn message_view(
    message: RwSignal<AgentMessage>,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> AnyView {
    let message_id = message.get_untracked().id;
    let is_thought = message.get_untracked().role == AgentRole::Thought;
    let is_tool = message.get_untracked().role == AgentRole::Tool;

    let role_label: AnyView = if is_tool {
        empty().into_any()
    } else {
        label(move || role_title(message.get().role).to_string())
            .style(move |s| {
                let config = config.get();
                let color = match message.get().role {
                    AgentRole::User => config.color(LapceColor::EDITOR_CARET),
                    AgentRole::Error => config.color(LapceColor::LAPCE_ERROR),
                    AgentRole::Notice => config.color(LapceColor::LAPCE_WARN),
                    AgentRole::Thought => config.color(LapceColor::EDITOR_DIM),
                    AgentRole::Tool | AgentRole::Plan => {
                        config.color(LapceColor::EDITOR_DIM)
                    }
                    AgentRole::Assistant => {
                        config.color(LapceColor::EDITOR_FOREGROUND)
                    }
                };
                s.font_size(config.ui.font_size() as f32 - 1.0)
                    .color(color)
                    .selectable(false)
            })
            .into_any()
    };

    let toggle: AnyView = if is_thought {
        let toggle_agent = agent.clone();
        let expanded = agent.expanded_thoughts;
        dyn_container(
            move || expanded.get().contains(&message_id),
            move |expanded| {
                let toggle_agent = toggle_agent.clone();
                action_button(
                    if expanded {
                        "Hide".to_string()
                    } else {
                        "Show".to_string()
                    },
                    move || toggle_agent.toggle_thought(message_id),
                    config,
                    false,
                )
                .into_any()
            },
        )
        .into_any()
    } else {
        empty().into_any()
    };

    let copy: AnyView = if is_tool {
        empty().into_any()
    } else {
        let copy_message = message;
        copy_icon(move || copy_message.get_untracked().content.clone(), config)
            .into_any()
    };

    let header = h_stack((
        role_label,
        empty().style(|s| s.flex_grow(1.0)),
        toggle,
        copy,
    ))
    .style(|s| s.width_full().items_center().gap(4.0));

    let body_agent = agent.clone();
    let body = dyn_container(
        move || {
            (
                message.get(),
                body_agent.expanded_thoughts.get().contains(&message_id),
            )
        },
        move |(message, expanded)| match message.role {
            AgentRole::Tool => {
                tool_message(message, agent.clone(), config).into_any()
            }
            AgentRole::Error => label(move || message.content.clone())
                .style(move |s| {
                    s.width_full()
                        .color(config.get().color(LapceColor::LAPCE_ERROR))
                        .selectable(true)
                })
                .into_any(),
            AgentRole::User => markdown_text(message.content, config).into_any(),
            AgentRole::Assistant => {
                markdown_text(message.content, config).into_any()
            }
            AgentRole::Thought => {
                if expanded {
                    markdown_text(message.content, config)
                        .style(move |s| {
                            s.color(config.get().color(LapceColor::EDITOR_DIM))
                        })
                        .into_any()
                } else {
                    let preview = preview_text(&message.content);
                    label(move || preview.clone())
                        .style(move |s| {
                            let config = config.get();
                            s.width_full()
                                .min_width(0.0)
                                .text_ellipsis()
                                .color(config.color(LapceColor::EDITOR_DIM))
                                .font_size(config.ui.font_size() as f32 - 1.0)
                                .selectable(false)
                        })
                        .into_any()
                }
            }
            AgentRole::Plan => markdown_text(message.content, config).into_any(),
            AgentRole::Notice => label(move || message.content.clone())
                .style(move |s| {
                    s.width_full()
                        .color(config.get().color(LapceColor::LAPCE_WARN))
                        .selectable(true)
                })
                .into_any(),
        },
    );

    stack((header, body))
        .style(move |s| {
            let config = config.get();
            s.width_full()
                .flex_col()
                .padding_horiz(10.0)
                .padding_vert(7.0)
                .border_bottom(1.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
                .apply_if(message.get().role == AgentRole::User, |s| {
                    s.background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
                })
        })
        .into_any()
}

fn role_title(role: AgentRole) -> &'static str {
    match role {
        AgentRole::User => "You",
        AgentRole::Assistant => "Agent",
        AgentRole::Thought => "Thinking",
        AgentRole::Tool => "Tool",
        AgentRole::Plan => "Plan",
        AgentRole::Notice => "Notice",
        AgentRole::Error => "Error",
    }
}

fn tool_message(
    message: AgentMessage,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let Some(tool) = message.tool else {
        return empty().into_any();
    };

    let status = tool.status.clone();
    let status_style = tool.status.clone();
    let kind = tool.kind.clone();
    let title = tool.title.clone();
    let content = tool.content.clone();
    let tool_id = message.id;
    let has_content = !content.is_empty();
    let body_content = content.clone();
    let copy_content = content;

    let toggle_agent = agent.clone();
    let expanded = agent.expanded_tools;
    let toggle = dyn_container(
        move || expanded.get().contains(&tool_id),
        move |expanded| {
            if !has_content {
                return empty().into_any();
            }
            let toggle_agent = toggle_agent.clone();
            action_button(
                if expanded {
                    "Hide".to_string()
                } else {
                    "Show".to_string()
                },
                move || toggle_agent.toggle_tool(tool_id),
                config,
                false,
            )
            .into_any()
        },
    );

    let copy: AnyView = if has_content {
        copy_icon(move || copy_content.clone(), config).into_any()
    } else {
        empty().into_any()
    };

    let body: AnyView = if has_content {
        let body_agent = agent.clone();
        dyn_container(
            move || body_agent.expanded_tools.get().contains(&tool_id),
            move |expanded| {
                if expanded {
                    markdown_text(body_content.clone(), config).into_any()
                } else {
                    let preview = preview_text(&body_content);
                    label(move || preview.clone())
                        .style(move |s| {
                            let config = config.get();
                            s.width_full()
                                .min_width(0.0)
                                .text_ellipsis()
                                .color(config.color(LapceColor::EDITOR_DIM))
                                .font_size(config.ui.font_size() as f32 - 1.0)
                                .selectable(false)
                        })
                        .into_any()
                }
            },
        )
        .into_any()
    } else {
        empty().into_any()
    };

    stack((
        h_stack((
            label(move || tool_icon(&kind)).style(move |s| {
                s.color(config.get().color(LapceColor::EDITOR_DIM))
                    .selectable(false)
            }),
            label(move || title.clone()).style(move |s| {
                let config = config.get();
                s.color(config.color(LapceColor::EDITOR_FOREGROUND))
                    .text_ellipsis()
                    .min_width(0.0)
                    .selectable(false)
            }),
            label(move || tool_status_label(&status)).style(move |s| {
                let config = config.get();
                let color = match status_style.as_str() {
                    "failed" => config.color(LapceColor::LAPCE_ERROR),
                    "completed" => config.color(LapceColor::EDITOR_DIM),
                    _ => config.color(LapceColor::LAPCE_WARN),
                };
                s.color(color)
                    .font_size(config.ui.font_size() as f32 - 1.0)
                    .selectable(false)
            }),
            empty().style(|s| s.flex_grow(1.0)),
            toggle,
            copy,
        ))
        .style(|s| s.width_full().items_center().gap(6.0)),
        body,
    ))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .flex_col()
            .padding(8.0)
            .gap(4.0)
            .border_radius(6.0)
            .border(1.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
    })
    .into_any()
}

fn tool_icon(kind: &str) -> &'static str {
    match kind {
        "read" => "◉",
        "edit" => "✎",
        "delete" => "✕",
        "move" => "→",
        "search" => "⌕",
        "execute" => "▸",
        "think" => "…",
        "fetch" => "⇣",
        _ => "•",
    }
}

fn tool_status_label(status: &str) -> &'static str {
    match status {
        "pending" => "waiting",
        "in_progress" => "running",
        "completed" => "done",
        "failed" => "failed",
        _ => "unknown",
    }
}

fn markdown_text(
    content: String,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    dyn_container(
        move || parse_markdown(&content, 1.6, &config.get()),
        move |blocks| {
            stack_from_iter(blocks.into_iter().map(move |block| {
                match block {
                    MarkdownContent::Text(text_layout) => {
                        rich_text(move || text_layout.clone())
                            .style(|s| s.width_full().min_width(0.0))
                            .into_any()
                    }
                    MarkdownContent::Image { .. } => empty().into_any(),
                    MarkdownContent::Separator => {
                        container(empty().style(move |s| {
                            s.width_full().height(1.0).margin_vert(4.0).background(
                                config.get().color(LapceColor::LAPCE_BORDER),
                            )
                        }))
                        .into_any()
                    }
                }
            }))
            .style(|s| s.flex_col().width_full().min_width(0.0))
        },
    )
}

fn permission_view(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    dyn_container(
        move || agent.permission.get(),
        move |permission| match permission {
            Some(permission) => {
                permission_card(permission, agent.clone(), config).into_any()
            }
            None => empty().into_any(),
        },
    )
}

fn permission_card(
    permission: PendingPermission,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let title = permission.title;
    let cancel_agent = agent.clone();

    stack((
        label(|| "Tool approval required".to_string()).style(move |s| {
            let config = config.get();
            s.font_size(config.ui.font_size() as f32 - 1.0)
                .color(config.color(LapceColor::LAPCE_WARN))
                .selectable(false)
        }),
        label(move || title.clone()).style(move |s| {
            s.width_full()
                .min_width(0.0)
                .text_ellipsis()
                .color(config.get().color(LapceColor::EDITOR_FOREGROUND))
                .selectable(true)
        }),
        stack((
            stack_from_iter(permission.options.into_iter().map(|option| {
                let agent = agent.clone();
                let option_id = option.option_id.clone();
                let reject =
                    matches!(option.kind.as_str(), "reject_once" | "reject_always");
                action_button(
                    option.name,
                    move || agent.resolve_permission(Some(option_id.clone())),
                    config,
                    !reject,
                )
            }))
            .style(|s| s.flex_col().gap(4.0)),
            action_button(
                "Cancel".to_string(),
                move || cancel_agent.resolve_permission(None),
                config,
                false,
            ),
        ))
        .style(|s| s.flex_col().gap(4.0)),
    ))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .flex_col()
            .padding(10.0)
            .gap(6.0)
            .border_top(1.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::PANEL_BACKGROUND))
    })
}

fn composer(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    focus: RwSignal<Focus>,
) -> impl View {
    let editor = agent.input.clone();
    let key_focus = AgentInputKeyPress {
        agent: agent.clone(),
        editor: editor.clone(),
    };

    let input = TextInputBuilder::new()
        .is_focused(move || focus.get() == Focus::Panel(PanelKind::Agent))
        .key_focus(key_focus)
        .build_editor(editor)
        .placeholder(|| "Ask the agent…".to_string())
        .style(|s| s.width_full().min_width(0.0))
        .keyboard_navigable();

    let input_id = input.id();
    let input = input.on_event_cont(EventListener::PointerDown, move |_| {
        focus.set(Focus::Panel(PanelKind::Agent));
        input_id.request_focus();
    });

    let config_agent = agent.clone();
    let send_agent = agent.clone();
    let send_input_id = input_id;

    stack((
        config_bar(config_agent, config),
        h_stack((
            container(
                scroll(input)
                    .scroll_style(|s| s.hide_bars(true))
                    .style(|s| s.width_full().min_height(0.0)),
            )
            .style(move |s| {
                let config = config.get();
                s.flex_grow(1.0f32)
                    .min_width(0.0)
                    .min_height(34.0)
                    .border(1.0)
                    .border_radius(6.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::EDITOR_BACKGROUND))
            }),
            action_button(
                "Send".to_string(),
                move || {
                    send_agent.submit_input();
                    send_input_id.request_focus();
                },
                config,
                true,
            ),
        ))
        .style(|s| s.width_full().items_end().gap(6.0)),
    ))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .flex_col()
            .gap(6.0)
            .padding(8.0)
            .border_top(1.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::EDITOR_BACKGROUND))
    })
    .debug_name("Agent Composer")
}

fn action_button(
    text: String,
    action: impl Fn() + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
    accent: bool,
) -> impl View {
    container(label(move || text.clone()))
        .on_click_stop(move |_| action())
        .style(move |s| {
            let config = config.get();
            let color = if accent {
                config.color(LapceColor::LAPCE_TAB_ACTIVE_UNDERLINE)
            } else {
                config.color(LapceColor::LAPCE_BORDER)
            };
            s.padding_horiz(9.0)
                .padding_vert(5.0)
                .border(1.0)
                .border_radius(5.0)
                .border_color(color)
                .cursor(CursorStyle::Pointer)
                .selectable(false)
                .hover(|s| {
                    s.background(config.color(LapceColor::PANEL_HOVERED_BACKGROUND))
                })
        })
}

#[derive(Clone)]
struct AgentInputKeyPress {
    agent: AgentData,
    editor: crate::editor::EditorData,
}

impl std::fmt::Debug for AgentInputKeyPress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentInputKeyPress").finish()
    }
}

impl KeyPressFocus for AgentInputKeyPress {
    fn get_mode(&self) -> Mode {
        self.editor.get_mode()
    }

    fn check_condition(&self, condition: Condition) -> bool {
        self.editor.check_condition(condition)
    }

    fn run_command(
        &self,
        command: &crate::command::LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        if matches!(&command.kind, CommandKind::Edit(EditCommand::InsertNewLine))
            && !mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::ALT)
        {
            self.agent.submit_input();
            return CommandExecuted::Yes;
        }

        self.editor.run_command(command, count, mods)
    }

    fn expect_char(&self) -> bool {
        self.editor.expect_char()
    }

    fn focus_only(&self) -> bool {
        self.editor.focus_only()
    }

    fn receive_char(&self, c: &str) {
        self.editor.receive_char(c);
    }
}
