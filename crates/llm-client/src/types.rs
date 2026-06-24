//! The vendor-neutral conversation types every backend speaks: [`Role`],
//! [`ImageData`], [`ContentBlock`], [`Message`], [`ToolDef`], [`ToolCall`], and
//! the parsed [`Completion`]. None of these depend on a particular wire format —
//! each backend maps them onto its own request/response shape.

use serde_json::Value;

/// Conversation role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// An image attached to a tool result, already base64-encoded for the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    /// Wire format identifier: "png", "jpeg", "gif", or "webp".
    pub format: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// A single block of message content.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentBlock {
    /// Plain text.
    Text(String),
    /// A standalone image in a user message (e.g. a rendered schematic/board the
    /// model should LOOK at). Distinct from [`ContentBlock::ToolResult`]'s
    /// attached images, which ride a finished tool call; this rides a plain
    /// vision prompt — the path the layout critic uses.
    Image(ImageData),
    /// A tool invocation the assistant requested.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of running a tool, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Images attached to the result (rendered schematics). Empty for
        /// text-only results.
        images: Vec<ImageData>,
    },
}

/// A message in the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Build a single-text user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Build a single-text assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Build a user message that asks the model to look at an image: the prompt
    /// text followed by the image itself. Used by the vision layout critic.
    pub fn user_with_image(text: impl Into<String>, image: ImageData) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into()), ContentBlock::Image(image)],
        }
    }
}

/// A tool the model may call.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's input.
    pub input_schema: Value,
}

/// A tool call extracted from a completion.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// The parsed result of a single completion request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    /// All text blocks, concatenated.
    pub text: String,
    /// Any tool calls the model requested.
    pub tool_calls: Vec<ToolCall>,
    /// Raw stop reason from the provider (e.g. `end_turn`, `tool_use`).
    pub stop_reason: String,
    /// Prompt tokens the provider reports for this call (0 when absent).
    /// This is the size of everything sent: system + history + tools.
    pub input_tokens: u64,
    /// Generated tokens the provider reports for this call (0 when absent).
    pub output_tokens: u64,
}
