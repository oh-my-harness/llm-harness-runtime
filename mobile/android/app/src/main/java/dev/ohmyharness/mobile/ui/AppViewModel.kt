package dev.ohmyharness.mobile.ui

import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.data.ConfigRepository
import dev.ohmyharness.mobile.data.isComplete
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import uniffi.llm_harness_mobile_core.MobileEvent
import uniffi.llm_harness_mobile_core.MobileSession
import uniffi.llm_harness_mobile_core.MobileSessionManager
import java.io.File

/**
 * Single ViewModel owning the session manager and live sessions. Screens
 * render [uiState] and call the public intents; events flow one way back.
 */
class AppViewModel(
    private val configRepository: ConfigRepository,
    private val filesDirPath: String,
) : ViewModel() {

    private val manager = MobileSessionManager(filesDirPath)
    private val live = mutableMapOf<String, MobileSession>()
    private val pumps = mutableMapOf<String, Job>()
    private var configCache: AppConfig? = null

    private val _uiState = MutableStateFlow(AppUiState())
    val uiState: StateFlow<AppUiState> = _uiState.asStateFlow()

    private fun update(reducer: (AppUiState) -> AppUiState) {
        _uiState.value = reducer(_uiState.value)
    }

    init {
        viewModelScope.launch {
            val cfg = configRepository.current()
            configCache = cfg
            _uiState.value = _uiState.value.copy(configComplete = cfg.isComplete(), config = cfg)
            if (cfg.isComplete()) {
                // Resume the most recent session when one exists; create only
                // on a fresh install. Creating per launch spawns a sandbox
                // on every cold start.
                val sessions = runCatching { manager.listSessions() }.getOrDefault(emptyList())
                if (sessions.isNotEmpty()) {
                    switchSession(sessions.first().id)
                } else {
                    connectWith(cfg)
                }
            }
        }
    }

    // ── Intents ────────────────────────────────────────────────────────────

    fun retry() {
        viewModelScope.launch { connectWith(currentConfig()) }
    }

    fun saveConfig(config: AppConfig) {
        viewModelScope.launch {
            configRepository.save(config)
            configCache = config
            _uiState.value = _uiState.value.copy(configComplete = config.isComplete(), config = config)
            if (config.isComplete()) {
                refreshSessions()
                connectWith(config)
            }
        }
    }

    fun send(text: String) {
        val session = activeSession() ?: return
        val id = _uiState.value.activeSessionId ?: return
        appendMessage(id, ChatMessage.User(text, key = nextKey()))
        session.send(text)
    }

    fun steer(text: String) {
        val session = activeSession() ?: return
        val id = _uiState.value.activeSessionId ?: return
        appendSystem(id, "steer: $text")
        try {
            session.steer(text)
        } catch (e: Exception) {
            appendSystem(id, "steer failed: ${e.message}")
        }
    }

    fun stop() {
        activeSession()?.abort()
    }

    fun newSession(name: String) {
        viewModelScope.launch {
            try {
                _uiState.value = _uiState.value.copy(connection = ConnectionState.Connecting)
                val session = manager.createSession(currentConfig().toMobileConfig(), name)
                val id = session.sessionId() ?: error("persisted session has no id")
                live[id] = session
                startPump(id, session)
                refreshSessions()
                setActive(id)
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(connection = ConnectionState.Failed(e.message ?: "create failed"))
                }
            }
        }
    }

    fun switchSession(sessionId: String) {
        viewModelScope.launch {
            try {
                if (live[sessionId] == null) {
                    _uiState.value = _uiState.value.copy(connection = ConnectionState.Connecting)
                    val session = manager.openSession(currentConfig().toMobileConfig(), sessionId)
                    live[sessionId] = session
                    startPump(sessionId, session)
                    replayHistory(sessionId, session)
                }
                setActive(sessionId)
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(connection = ConnectionState.Failed(e.message ?: "resume failed"))
                }
            }
        }
    }

    fun deleteSession(sessionId: String) {
        viewModelScope.launch {
            try {
                live.remove(sessionId)?.let { runCatching { it.shutdown() } }
                pumps.remove(sessionId)?.cancel()
                manager.deleteSession(sessionId)
                _uiState.update { state ->
                    state.copy(
                        sessions = state.sessions.filterNot { it.id == sessionId },
                        activeSessionId = state.activeSessionId.takeUnless { it == sessionId },
                    )
                }
            } catch (e: Exception) {
                appendSystem(activeSessionId() ?: return@launch, "delete failed: ${e.message}")
            }
        }
    }

    // ── Internals ──────────────────────────────────────────────────────────

    private var keySeq = 0L

    private fun nextKey(): String = "m${keySeq++}"

    private fun activeSession(): MobileSession? =
        _uiState.value.activeSessionId?.let { live[it] }

    private fun activeSessionId(): String? = _uiState.value.activeSessionId

    private suspend fun currentConfig(): AppConfig =
        configCache ?: configRepository.current().also { configCache = it }

    private fun setActive(id: String) {
        _uiState.update { it.copy(activeSessionId = id) }
    }

    private suspend fun connectWith(config: AppConfig) {
        _uiState.update { it.copy(connection = ConnectionState.Connecting) }
        try {
            // The chat UI needs a persisted session (active id + history);
            // the bare connect() FFI creates an ad-hoc in-memory session.
            val session = manager.createSession(config.toMobileConfig(), "Session 1")
            val id = session.sessionId() ?: error("persisted session has no id")
            live[id] = session
            startPump(id, session)
            replayHistory(id, session)
            refreshSessions()
            _uiState.update { it.copy(activeSessionId = id) }
            _uiState.update { it.copy(connection = ConnectionState.Connected(config.trustProfile)) }
        } catch (e: Exception) {
            _uiState.update {
                it.copy(connection = ConnectionState.Failed(e.message ?: "connect failed"))
            }
        }
    }

    private fun startPump(sessionId: String, session: MobileSession) {
        pumps[sessionId]?.cancel()
        pumps[sessionId] = viewModelScope.launch {
            while (true) {
                val event = session.pollEvent()
                if (event == null) delay(100) else onEvent(sessionId, event)
            }
        }
    }

    private fun onEvent(sessionId: String, event: MobileEvent) {
        when (event) {
            is MobileEvent.TextDelta -> appendDelta(sessionId, event.text)
            is MobileEvent.ToolStarted -> appendMessage(
                sessionId,
                ChatMessage.Tool(
                    name = event.name, summary = "running…", fullOutput = null,
                    isError = false, key = nextKey(),
                ),
            )
            is MobileEvent.ToolFinished -> updateLastTool(sessionId) {
                it.copy(
                    summary = event.summary,
                    fullOutput = event.fullOutput.takeIf { o -> o.isNotBlank() },
                    isError = isErrorSummary(event.summary),
                )
            }
            is MobileEvent.Failed -> appendSystem(sessionId, event.message)
            is MobileEvent.RunFinished -> finishStreaming(sessionId)
            is MobileEvent.RunAborted -> {
                appendSystem(sessionId, "run aborted")
                finishStreaming(sessionId)
            }
        }
    }

    private fun isErrorSummary(summary: String): Boolean =
        summary.startsWith("Command exited with code") ||
            summary.startsWith("error", ignoreCase = true)

    private fun appendDelta(sessionId: String, delta: String) {
        _uiState.update { state ->
            val list = state.messages[sessionId].orEmpty().toMutableList()
            val last = list.lastOrNull()
            if (last is ChatMessage.Assistant && last.isStreaming) {
                list[list.lastIndex] = last.copy(text = last.text + delta)
            } else {
                list.add(ChatMessage.Assistant(text = delta, isStreaming = true, key = nextKey()))
            }
            state.copy(messages = state.messages + (sessionId to list))
        }
    }

    private fun finishStreaming(sessionId: String) {
        _uiState.update { state ->
            val list = state.messages[sessionId]?.toMutableList() ?: return@update state
            val last = list.lastOrNull()
            if (last is ChatMessage.Assistant && last.isStreaming) {
                list[list.lastIndex] = last.copy(isStreaming = false)
            }
            state.copy(messages = state.messages + (sessionId to list))
        }
        viewModelScope.launch { refreshSessions() }
    }

    private fun appendMessage(sessionId: String, message: ChatMessage) {
        _uiState.update { state ->
            val list = state.messages[sessionId].orEmpty() + message
            state.copy(messages = state.messages + (sessionId to list))
        }
    }

    private fun appendSystem(sessionId: String, text: String) {
        appendMessage(sessionId, ChatMessage.SystemNotice(text, key = nextKey()))
    }

    private fun updateLastTool(
        sessionId: String,
        transform: (ChatMessage.Tool) -> ChatMessage.Tool,
    ) {
        _uiState.update { state ->
            val list = state.messages[sessionId]?.toMutableList() ?: return@update state
            for (i in list.indices.reversed()) {
                val m = list[i]
                if (m is ChatMessage.Tool) {
                    list[i] = transform(m)
                    break
                }
            }
            state.copy(messages = state.messages + (sessionId to list))
        }
    }

    private fun AppConfig.toMobileConfig(): uniffi.llm_harness_mobile_core.MobileConfig =
        uniffi.llm_harness_mobile_core.MobileConfig(
            gatewayUrl = gatewayUrl,
            gatewayToken = gatewayToken,
            apiKey = apiKey,
            model = model,
            llmBaseUrl = llmBaseUrl,
            provider = provider,
            allowInsecureHttp = allowInsecureHttp,
            trustProfile = trustProfile,
        )

    private fun refreshSessions() {
        viewModelScope.launch {
            runCatching { manager.listSessions() }.onSuccess { list ->
                _uiState.update { it.copy(sessions = list) }
            }
        }
    }

    private fun replayHistory(sessionId: String, session: MobileSession) {
        viewModelScope.launch {
            try {
                val replayed = session.history().map { m ->
                    when (m.role) {
                        "user" -> ChatMessage.User(m.text, key = nextKey())
                        "assistant" ->
                            ChatMessage.Assistant(m.text, isStreaming = false, key = nextKey())
                        "tool" -> ChatMessage.Tool(
                            name = "tool", summary = m.text, fullOutput = null,
                            isError = m.isError, key = nextKey(),
                        )
                        else -> ChatMessage.SystemNotice(m.text, key = nextKey())
                    }
                }
                _uiState.update { state ->
                    state.copy(messages = state.messages + (sessionId to replayed))
                }
            } catch (e: Exception) {
                appendSystem(sessionId, "history load failed: ${e.message}")
            }
        }
    }

    override fun onCleared() {
        pumps.values.forEach { it.cancel() }
        runCatching { manager.close() }
        super.onCleared()
    }

    companion object {
        fun factory(
            configRepository: ConfigRepository,
            filesDir: java.io.File,
        ): ViewModelProvider.Factory = object : ViewModelProvider.Factory {
            @Suppress("UNCHECKED_CAST")
            override fun <T : ViewModel> create(modelClass: Class<T>): T =
                AppViewModel(configRepository, filesDir.absolutePath) as T
        }
    }
}
