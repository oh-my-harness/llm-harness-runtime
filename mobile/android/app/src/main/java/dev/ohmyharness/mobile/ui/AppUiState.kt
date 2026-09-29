package dev.ohmyharness.mobile.ui

import uniffi.llm_harness_mobile_core.SessionSummary

/**
 * Immutable UI state for the whole app. The ViewModel is the single source of
 * truth; screens render from this and call back into [AppViewModel] intents.
 */
data class AppUiState(
    val configComplete: Boolean = false,
    val connection: ConnectionState = ConnectionState.Idle,
    val sessions: List<SessionSummary> = emptyList(),
    val activeSessionId: String? = null,
    /** Per-session transcript: replayed history + live events, keyed by session id. */
    val messages: Map<String, List<ChatMessage>> = emptyMap(),
)

sealed interface ConnectionState {
    data object Idle : ConnectionState
    data object Connecting : ConnectionState
    data class Connected(val trustProfile: String) : ConnectionState
    data class Failed(val message: String) : ConnectionState
}

/** One rendered chat line. Keys are unique per conversation element. */
sealed interface ChatMessage {
    val key: String

    data class User(val text: String, override val key: String) : ChatMessage
    data class Assistant(
        val text: String,
        val isStreaming: Boolean,
        override val key: String,
    ) : ChatMessage

    data class Tool(
        val name: String,
        val summary: String,
        val fullOutput: String?,
        val isError: Boolean,
        override val key: String,
    ) : ChatMessage

    data class SystemNotice(val text: String, override val key: String) : ChatMessage
}
