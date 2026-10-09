package dev.ohmyharness.mobile.ui

import uniffi.llm_harness_mobile_core.SessionSummary

/**
 * Immutable UI state for the whole app. The ViewModel is the single source of
 * truth; screens render from this and call back into [AppViewModel] intents.
 */
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.data.Machine

data class AppUiState(
    val configComplete: Boolean = false,
    val config: AppConfig = AppConfig(),
    val connection: ConnectionState = ConnectionState.Idle,
    val sessions: List<SessionSummary> = emptyList(),
    val activeSessionId: String? = null,
    /** Registered machines (first layer of the 3-level nav). */
    val machines: List<Machine> = emptyList(),
    /** Unread message counts per session id; cleared on entering a chat. */
    val unread: Map<String, Int> = emptyMap(),
    /** How tool executions render (Settings > Tool output). */
    val toolDisplay: ToolDisplay = ToolDisplay.Tail,
    /** Per-session transcript: replayed history + live events, keyed by session id. */
    val messages: Map<String, List<ChatMessage>> = emptyMap(),
    /** Global type-scale multiplier from Settings; theme applies it. */
    val fontScale: Float = 1.0f,
)

/** How tool cards render in the chat (Settings preference). */
enum class ToolDisplay {
    /** Hide tool cards entirely; show only conversation text. */
    Hidden,
    /** One summary line, expandable to the full output. */
    Summary,
    /** Collapsed tail preview (~8 lines) while running, expandable. */
    Tail,
}

sealed interface ConnectionState {
    data object Idle : ConnectionState
    data object Connecting : ConnectionState
    data class Connected(val trustProfile: String) : ConnectionState
    data class Failed(val message: String) : ConnectionState
}

/** One rendered chat line. Keys are unique per conversation element. */
sealed interface ChatMessage {
    val key: String

    data class User(val text: String, override val key: String, val queued: Boolean = false) : ChatMessage
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
        /** True between ToolStarted and ToolFinished; renders a live spinner. */
        val running: Boolean = false,
        val startedAtMs: Long = 0L,
        /** Harness tool_use_id; ToolFinished matches on it (parallel calls). */
        val toolUseId: String? = null,
        /** Live streaming tail (last ~8 lines) while running; null when none. */
        val liveTail: String? = null,
    ) : ChatMessage

    data class SystemNotice(val text: String, override val key: String) : ChatMessage
}
