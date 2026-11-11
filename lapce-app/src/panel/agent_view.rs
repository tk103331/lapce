use std::{rc::Rc, sync::Arc};

use floem::views::dropdown::Dropdown;
use floem::{
    AnyView, IntoView, View,
    event::EventListener,
    keyboard::Modifiers,
    kurbo::Point,
    reactive::{
        ReadSignal, RwSignal, SignalGet, SignalUpdate, create_memo, create_rw_signal,
    },
    style::CursorStyle,
    views::{
        Decorators, container, dyn_container, dyn_stack, empty, h_stack, label,
        rich_text, scroll::scroll, stack, stack_from_iter,
    },
};
use lapce_core::{command::EditCommand, mode::Mode};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    agent::{
        AgentChoiceState, AgentConfigOption, AgentConfigOptionKind,
        AgentConfigValue, AgentData, AgentMessage, AgentRole, AgentStatus,
        PendingPermission,
    },
    app::clickable_icon,
    command::{CommandExecuted, CommandKind},
    config::{LapceConfig, color::LapceColor, icon::LapceIcons},
    editor::EditorData,
    keypress::{KeyPressFocus, condition::Condition},
    markdown::{MarkdownContent, parse_markdown},
    text_input::TextInputBuilder,
    window_tab::{Focus, WindowTabData},
};

trait SelectorItem: Clone + std::fmt::Display + 'static {
    fn selector_label(&self) -> String;
}

#[derive(Clone)]
struct ProfileChoice {
    name: String,
}

impl std::fmt::Display for ProfileChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ▾", self.name)
    }
}

impl SelectorItem for ProfileChoice {
    fn selector_label(&self) -> String {
        self.name.clone()
    }
}

#[derive(Clone)]
struct ModelChoice {
    id: String,
    name: String,
}

impl std::fmt::Display for ModelChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ▾", self.name)
    }
}

impl SelectorItem for ModelChoice {
    fn selector_label(&self) -> String {
        self.name.clone()
    }
}

#[derive(Clone)]
struct ConfigValueChoice {
    value: String,
    name: String,
}

impl std::fmt::Display for ConfigValueChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ▾", self.name)
    }
}

impl SelectorItem for ConfigValueChoice {
    fn selector_label(&self) -> String {
        self.name.clone()
    }
}

fn selector<T: SelectorItem>(
    current: T,
    items: im::Vector<T>,
    on_accept: impl Fn(T) + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> Dropdown<T> {
    let current = current.clone();
    Dropdown::new(move || current.clone(), items)
        .on_accept(on_accept)
        .main_view(move |item| {
            let text = item.to_string();
            label(move || text.clone())
                .style(move |s| {
                    let config = config.get();
                    s.max_width(160.0)
                        .min_width(0.0)
                        .text_ellipsis()
                        .font_size(config.ui.font_size() as f32 - 1.0)
                        .color(config.color(LapceColor::EDITOR_FOREGROUND))
                })
                .into_any()
        })
        .list_item_view(move |item| {
            let text = item.selector_label();
            label(move || text.clone())
                .style(move |s| {
                    let config = config.get();
                    s.width_full()
                        .padding_horiz(8.0)
                        .padding_vert(4.0)
                        .font_size(config.ui.font_size() as f32 - 1.0)
                        .color(config.color(LapceColor::EDITOR_FOREGROUND))
                        .hover(|s| {
                            s.background(
                                config.color(LapceColor::PANEL_HOVERED_BACKGROUND),
                            )
                        })
                })
                .into_any()
        })
        .style(move |s| {
            let config = config.get();
            s.padding_horiz(6.0)
                .padding_vert(2.0)
                .min_width(0.0)
                .max_width_pct(100.0)
                .border(1.0)
                .border_radius(4.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
                .font_size(config.ui.font_size() as f32 - 1.0)
                .color(config.color(LapceColor::EDITOR_FOREGROUND))
                .cursor(CursorStyle::Pointer)
                .hover(|s| {
                    s.background(config.color(LapceColor::PANEL_HOVERED_BACKGROUND))
                })
        })
}

fn option_label(text: String, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    label(move || text.clone()).style(move |s| {
        let config = config.get();
        s.width(64.0)
            .min_width(0.0)
            .text_ellipsis()
            .font_size(config.ui.font_size() as f32 - 1.0)
            .color(config.color(LapceColor::EDITOR_DIM))
            .selectable(false)
    })
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
        conversation(agent.clone(), config),
        permission_view(agent.clone(), config),
        composer(agent, config, focus),
    ))
    .on_event_cont(EventListener::PointerDown, move |_| {
        panel_focus.set(Focus::Panel(PanelKind::Agent));
    })
    .style(|s| s.flex_col().size_full().min_height(0.0))
    .debug_name("Agent Panel")
}

fn header(agent: AgentData, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    let is_busy = {
        let agent = agent.clone();
        create_memo(move |_| agent.status.get().is_busy())
    };

    let name_agent = agent.clone();
    let status_label_agent = agent.clone();
    let status_style_agent = agent.clone();
    let new_session = agent.clone();
    let stop = agent.clone();
    let restart = agent.clone();

    h_stack((
        stack((
            label(move || {
                name_agent
                    .agent_name
                    .get()
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
                    AgentStatus::WaitingForPermission => {
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
        ))
        .style(|s| s.flex_col().flex_grow(1.0f32).min_width(0.0)),
        h_stack((
            clickable_icon(
                || LapceIcons::ADD,
                move || new_session.new_session(),
                || false,
                move || is_busy.get(),
                || "New session",
                config,
            ),
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

fn config_bar(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    focus: RwSignal<Focus>,
) -> impl View {
    stack((
        profile_selector(agent.clone(), config),
        add_agent_form(agent.clone(), config, focus),
        options_panel(agent, config),
    ))
    .style(|s| s.flex_col().width_full().gap(4.0))
}

fn profile_selector(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    dyn_container(
        move || (agent.profiles.get(), agent.active_profile.get()),
        move |(profiles, active)| {
            let items: im::Vector<ProfileChoice> = profiles
                .iter()
                .map(|profile| ProfileChoice {
                    name: profile.name.clone(),
                })
                .collect();
            let Some(current) = items
                .iter()
                .find(|choice| choice.name == active)
                .cloned()
                .or_else(|| items.get(0).cloned())
            else {
                return empty().into_any();
            };

            let select_agent = agent.clone();
            let add_agent = agent.clone();
            let dropdown = selector(
                current,
                items,
                move |choice| select_agent.select_profile(choice.name),
                config,
            );

            h_stack((
                option_label("Agent".to_string(), config),
                dropdown,
                action_button(
                    "+".to_string(),
                    move || add_agent.toggle_add_agent(),
                    config,
                    false,
                ),
            ))
            .style(|s| s.width_full().items_center().gap(6.0))
            .into_any()
        },
    )
}

fn add_agent_form(
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
    focus: RwSignal<Focus>,
) -> impl View {
    dyn_container(
        move || agent.show_add_agent.get(),
        move |show| {
            if !show {
                return empty().into_any();
            }

            let submit_agent = agent.clone();
            let cancel_agent = agent.clone();

            stack((
                form_input(
                    agent.new_agent_name_input.clone(),
                    "Agent name",
                    focus,
                    config,
                ),
                form_input(
                    agent.new_agent_command_input.clone(),
                    "Command, e.g. gemini --experimental-acp",
                    focus,
                    config,
                ),
                h_stack((
                    action_button(
                        "Add".to_string(),
                        move || submit_agent.submit_new_agent(),
                        config,
                        true,
                    ),
                    action_button(
                        "Cancel".to_string(),
                        move || cancel_agent.show_add_agent.set(false),
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
                    .gap(4.0)
                    .padding(6.0)
                    .border(1.0)
                    .border_radius(5.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::PANEL_BACKGROUND))
            })
            .into_any()
        },
    )
}

fn form_input(
    editor: EditorData,
    placeholder: &'static str,
    focus: RwSignal<Focus>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
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

            if let Some(models) = models {
                if let Some(row) = model_row(models, agent.clone(), config) {
                    rows.push(row);
                }
            }
            if let Some(modes) = modes {
                if let Some(row) = mode_row(modes, agent.clone(), config) {
                    rows.push(row);
                }
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
                stack_from_iter(rows).style(|s| s.flex_col().width_full().gap(4.0)),
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
) -> Option<AnyView> {
    let items: im::Vector<ModelChoice> = state
        .available
        .into_iter()
        .map(|model| ModelChoice {
            id: model.id,
            name: model.name,
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    let current = items
        .iter()
        .find(|choice| choice.id == state.current)
        .cloned()
        .or_else(|| items.get(0).cloned())?;

    let dropdown = selector(
        current,
        items,
        move |choice| agent.select_model(choice.id),
        config,
    );

    Some(
        h_stack((option_label("Model".to_string(), config), dropdown))
            .style(|s| s.width_full().items_center().gap(6.0))
            .into_any(),
    )
}

fn mode_row(
    state: AgentChoiceState,
    agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> Option<AnyView> {
    let items: im::Vector<ModelChoice> = state
        .available
        .into_iter()
        .map(|mode| ModelChoice {
            id: mode.id,
            name: mode.name,
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    let current = items
        .iter()
        .find(|choice| choice.id == state.current)
        .cloned()
        .or_else(|| items.get(0).cloned())?;

    let dropdown = selector(
        current,
        items,
        move |choice| agent.select_mode(choice.id),
        config,
    );

    Some(
        h_stack((option_label("Mode".to_string(), config), dropdown))
            .style(|s| s.width_full().items_center().gap(6.0))
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
            let items: im::Vector<ConfigValueChoice> = options
                .into_iter()
                .map(|option| ConfigValueChoice {
                    value: option.value,
                    name: option.name,
                })
                .collect();
            if items.is_empty() {
                return None;
            }
            let current = items
                .iter()
                .find(|choice| choice.value == current)
                .cloned()
                .or_else(|| items.get(0).cloned())?;

            let config_id = id;
            let dropdown = selector(
                current,
                items,
                move |choice| {
                    agent.set_config_option(
                        &config_id,
                        AgentConfigValue::Select(choice.value),
                    )
                },
                config,
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
    _agent: AgentData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> AnyView {
    stack((
        label(move || role_title(message.get().role).to_string()).style(move |s| {
            let config = config.get();
            let color = match message.get().role {
                AgentRole::User => config.color(LapceColor::EDITOR_CARET),
                AgentRole::Error => config.color(LapceColor::LAPCE_ERROR),
                AgentRole::Notice => config.color(LapceColor::LAPCE_WARN),
                AgentRole::Thought => config.color(LapceColor::EDITOR_DIM),
                AgentRole::Tool | AgentRole::Plan => {
                    config.color(LapceColor::EDITOR_DIM)
                }
                AgentRole::Assistant => config.color(LapceColor::EDITOR_FOREGROUND),
            };
            s.font_size(config.ui.font_size() as f32 - 1.0)
                .color(color)
                .selectable(false)
                .padding_bottom(2.0)
                .apply_if(message.get().role == AgentRole::Tool, |s| s.hide())
        }),
        dyn_container(
            move || message.get(),
            move |message| match message.role {
                AgentRole::Tool => tool_message(message, config).into_any(),
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
                AgentRole::Thought => markdown_text(message.content, config)
                    .style(move |s| {
                        s.color(config.get().color(LapceColor::EDITOR_DIM))
                    })
                    .into_any(),
                AgentRole::Plan => markdown_text(message.content, config).into_any(),
                AgentRole::Notice => label(move || message.content.clone())
                    .style(move |s| {
                        s.width_full()
                            .color(config.get().color(LapceColor::LAPCE_WARN))
                            .selectable(true)
                    })
                    .into_any(),
            },
        ),
    ))
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
        ))
        .style(|s| s.items_center().gap(6.0)),
        if content.is_empty() {
            empty().into_any()
        } else {
            markdown_text(content, config).into_any()
        },
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
        label(|| "Permission required".to_string()).style(move |s| {
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
        config_bar(config_agent, config, focus),
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
