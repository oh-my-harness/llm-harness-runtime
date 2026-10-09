package dev.ohmyharness.mobile.ui

import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.data.ConfigRepository
import dev.ohmyharness.mobile.data.isComplete
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import uniffi.llm_harness_mobile_core.MobileEvent
import uniffi.llm_harness_mobile_core.MobileSession
import dev.ohmyharness.mobile.task.TaskKeepaliveService
import uniffi.llm_harness_mobile_core.MobileSessionManager
import java.io.File

/**
 * Single ViewModel owning the session manager and live sessions. Screens
 * render [uiState] and call the public intents; events flow one way back.
 */
class AppViewModel(
    private val configRepository: ConfigRepository,
    private val machineRepository: dev.ohmyharness.mobile.data.MachineRepository,
    private val llmRepository: dev.ohmyharness.mobile.data.LlmRepository,
    private val filesDirPath: String,
    private val appContext: android.content.Context,
) : ViewModel() {

    private val manager = MobileSessionManager(filesDirPath)
    private val live = mutableMapOf<String, MobileSession>()
    private val pumps = mutableMapOf<String, Job>()
    private var configCache: AppConfig? = null

    /** Machine used for new connections; Batch 3 makes this user-selectable. */
    private var activeMachine: dev.ohmyharness.mobile.data.Machine? = null

    private val _uiState = MutableStateFlow(AppUiState())
    val uiState: StateFlow<AppUiState> = _uiState.asStateFlow()

    private fun update(reducer: (AppUiState) -> AppUiState) {
        _uiState.value = reducer(_uiState.value)
    }

    init {
        viewModelScope.launch(Dispatchers.IO) {
            configRepository.migrateLegacy(machineRepository, llmRepository)
            val cfg = configRepository.current()
            configCache = cfg
            val machines = machineRepository.current()
            activeMachine = machines.firstOrNull()
            _uiState.value = _uiState.value.copy(
                configComplete = cfg.isComplete(), config = cfg, fontScale = cfg.fontScale,
                machines = machines,
                toolDisplay = ToolDisplay.entries.firstOrNull { it.name.lowercase() == cfg.toolDisplay }
                    ?: ToolDisplay.Tail,
            )
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
        viewModelScope.launch(Dispatchers.IO) { connectWith(currentConfig()) }
    }

    fun saveConfig(config: AppConfig) {
        viewModelScope.launch(Dispatchers.IO) {
            configRepository.save(config)
            configCache = config
            _uiState.value = _uiState.value.copy(
                configComplete = config.isComplete(), config = config, fontScale = config.fontScale,
                toolDisplay = ToolDisplay.entries.firstOrNull { it.name.lowercase() == config.toolDisplay }
                    ?: ToolDisplay.Tail,
            )
            if (config.isComplete()) {
                refreshSessions()
                connectWith(config)
            }
        }
    }

    fun send(text: String) {
        val id = _uiState.value.activeSessionId ?: return
        val existing = activeSession()
        if (existing != null) {
            if (isStreaming(id)) {
                // Queued steer: lands mid-run; visual mark clears on the
                // next assistant delta (see appendDelta).
                appendMessage(id, ChatMessage.User(text, key = nextKey(), queued = true))
                try {
                    sessionOf(id)?.steer(text)
                } catch (e: Exception) {
                    appendSystem(id, "queue failed: ${e.message}")
                }
                return
            }
            appendMessage(id, ChatMessage.User(text, key = nextKey()))
            existing.send(text)
            return
        }
        // Lazy connect: first send in a resumed session opens the gateway
        // sandbox, then dispatches. openSession replays history itself, so
        // avoid double-appending the user line here.
        viewModelScope.launch(Dispatchers.IO) {
            try {
                _uiState.value = _uiState.value.copy(connection = ConnectionState.Connecting)
                val session = manager.openSession(currentConfig().toMobileConfig(), id)
                live[id] = session
                startPump(id, session)
                appendMessage(id, ChatMessage.User(text, key = nextKey()))
                session.send(text)
                _uiState.value = _uiState.value.copy(
                    connection = ConnectionState.Connected(activeMachine?.trustProfile ?: "restricted"),
                )
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(connection = ConnectionState.Failed(e.message ?: "connect failed"))
                }
            }
        }
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
        viewModelScope.launch(Dispatchers.IO) {
            try {
                _uiState.value = _uiState.value.copy(connection = ConnectionState.Connecting)
                val session = manager.createSession(currentConfig().toMobileConfig(), name)
                val id = session.sessionId() ?: error("persisted session has no id")
                live[id] = session
                startPump(id, session)
                refreshSessions()
                setActive(id)
                _uiState.value = _uiState.value.copy(
                    connection = ConnectionState.Connected(activeMachine?.trustProfile ?: "restricted"),
                )
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(connection = ConnectionState.Failed(e.message ?: "create failed"))
                }
            }
        }
    }

    fun switchSession(sessionId: String) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                if (live[sessionId] == null) {
                    // Lazy connect: entering a chat reads persisted history
                    // only; the gateway connection happens on first send.
                    val replayed = manager.readHistory(sessionId)
                    _uiState.update { s ->
                        s.copy(messages = s.messages + (sessionId to replayed.map(::toChatMessage)))
                    }
                }
                setActive(sessionId)
                _uiState.value = _uiState.value.copy(
                    connection = ConnectionState.Connected(activeMachine?.trustProfile ?: "restricted"),
                )
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(connection = ConnectionState.Failed(e.message ?: "resume failed"))
                }
            }
        }
    }

    fun deleteSession(sessionId: String) {
        viewModelScope.launch(Dispatchers.IO) {
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

    private fun sessionOf(id: String): MobileSession? = live[id]

    private fun isStreaming(sessionId: String): Boolean {
        val list = _uiState.value.messages[sessionId] ?: return false
        return list.lastOrNull() is ChatMessage.Assistant && (list.lastOrNull() as ChatMessage.Assistant).isStreaming
    }

    private fun activeSessionId(): String? = _uiState.value.activeSessionId

    private suspend fun currentConfig(): AppConfig =
        configCache ?: configRepository.current().also { configCache = it }

    private fun setActive(id: String) {
        _uiState.update { it.copy(activeSessionId = id, unread = it.unread - id) }
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
            _uiState.update {
                it.copy(connection = ConnectionState.Connected(activeMachine?.trustProfile ?: "restricted"))
            }
        } catch (e: Exception) {
            _uiState.update {
                it.copy(connection = ConnectionState.Failed(e.message ?: "connect failed"))
            }
        }
    }

    private fun startPump(sessionId: String, session: MobileSession) {
        pumps[sessionId]?.cancel()
        pumps[sessionId] = viewModelScope.launch(Dispatchers.IO) {
            while (true) {
                val event = session.pollEvent()
                if (event == null) delay(100) else onEvent(sessionId, event)
            }
        }
    }

    private fun onEvent(sessionId: String, event: MobileEvent) {
        // Unread badge: count events for sessions not on screen.
        if (sessionId != _uiState.value.activeSessionId) {
            update { s -> s.copy(unread = s.unread + (sessionId to (s.unread[sessionId] ?: 0) + 1)) }
        }
        // A tool card or run boundary must land after any buffered text.
        if (event !is MobileEvent.TextDelta) flushPendingDelta(sessionId)
        when (event) {
            is MobileEvent.TextDelta -> appendDelta(sessionId, event.text)
            is MobileEvent.ToolStarted -> {
                appendMessage(
                    sessionId,
                    ChatMessage.Tool(
                        name = event.name, summary = "running…", fullOutput = null,
                        isError = false, key = nextKey(), running = true,
                        startedAtMs = System.currentTimeMillis(),
                    ),
                )
                refreshKeepalive()
            }
            is MobileEvent.ToolUpdate -> updateTool(sessionId, event.toolUseId) {
                it.copy(liveTail = event.text)
            }
            is MobileEvent.ToolFinished -> updateTool(sessionId, event.toolUseId) {
                it.copy(
                    summary = event.summary,
                    fullOutput = event.fullOutput.takeIf { o -> o.isNotBlank() },
                    isError = isErrorSummary(event.summary),
                    running = false,
                    liveTail = null,
                )
            }
            is MobileEvent.Failed -> {
                appendSystem(sessionId, event.message)
                onRunEnded(sessionId, failed = true)
            }
            is MobileEvent.RunFinished -> {
                finishStreaming(sessionId)
                onRunEnded(sessionId, failed = false)
            }
            is MobileEvent.RunAborted -> {
                appendSystem(sessionId, "run aborted")
                finishStreaming(sessionId)
                onRunEnded(sessionId, failed = false)
            }
        }
    }

    /** Completion notification for background tasks + keepalive refresh. */
    private fun onRunEnded(sessionId: String, failed: Boolean) {
        if (sessionId != _uiState.value.activeSessionId) {
            TaskKeepaliveService.notifyTaskFinished(
                appContext,
                "Task finished" + if (failed) " (failed)" else "",
                lastAssistantLine(sessionId),
            )
        }
        refreshKeepalive()
    }

    private fun lastAssistantLine(sessionId: String): String =
        (_uiState.value.messages[sessionId]
            ?.lastOrNull { it is ChatMessage.Assistant } as? ChatMessage.Assistant)
            ?.text?.lines()?.firstOrNull()?.take(80)
            ?: "no output"

    /** Keepalive foreground service tracks how many sessions are mid-run. */
    private fun refreshKeepalive() {
        val running = _uiState.value.messages.count { (_, list) ->
            (list.lastOrNull() as? ChatMessage.Assistant)?.isStreaming == true
        }
        if (running > 0) {
            TaskKeepaliveService.start(appContext, running)
        } else {
            TaskKeepaliveService.stop(appContext)
        }
    }

    private fun isErrorSummary(summary: String): Boolean =
        summary.startsWith("Command exited with code") ||
            summary.startsWith("error", ignoreCase = true)

    /** Deltas buffered between flush frames; guarded by the main thread only. */
    private val pendingDelta = StringBuilder()
    private var deltaFlushJob: Job? = null

    /**
     * Buffer a streaming delta and flush buffered text every [DELTA_FRAME_MS].
     * Markdown re-parses per emission, so emitting at token rate would re-parse
     * dozens of times a second; one frame per [DELTA_FRAME_MS] is visually
     * indistinguishable and dramatically cheaper.
     */
    private fun appendDelta(sessionId: String, delta: String) {
        pendingDelta.append(delta)
        if (deltaFlushJob?.isActive == true) return
        deltaFlushJob = viewModelScope.launch {
            delay(DELTA_FRAME_MS)
            flushPendingDelta(sessionId)
        }
    }

    /** Emit buffered text as one assistant delta (no-op when nothing buffered). */
    private fun flushPendingDelta(sessionId: String) {
        val text = pendingDelta.toString().ifEmpty { return }
        pendingDelta.clear()
        _uiState.update { state ->
            val list = state.messages[sessionId].orEmpty().toMutableList()
            val last = list.lastOrNull()
            // Assistant activity consumes queued steers: clear the marks.
            val cleared = list.map { m -> if (m is ChatMessage.User && m.queued) m.copy(queued = false) else m }
            list.clear()
            list.addAll(cleared)
            if (last is ChatMessage.Assistant && last.isStreaming) {
                list[list.lastIndex] = last.copy(text = last.text + text)
            } else {
                list.add(ChatMessage.Assistant(text = text, isStreaming = true, key = nextKey()))
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
        viewModelScope.launch(Dispatchers.IO) { refreshSessions() }
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

    private fun updateTool(
        sessionId: String,
        toolUseId: String?,
        transform: (ChatMessage.Tool) -> ChatMessage.Tool,
    ) {
        _uiState.update { state ->
            val list = state.messages[sessionId]?.toMutableList() ?: return@update state
            var target = -1
            // Match by harness tool_use_id (parallel calls race); fall back
            // to the last still-running card when the id is unknown.
            if (toolUseId != null) {
                for (i in list.indices.reversed()) {
                    val m = list[i]
                    if (m is ChatMessage.Tool && m.toolUseId == toolUseId) {
                        target = i
                        break
                    }
                }
            }
            if (target < 0) {
                for (i in list.indices.reversed()) {
                    val m = list[i]
                    if (m is ChatMessage.Tool && m.running) {
                        target = i
                        break
                    }
                }
            }
            if (target >= 0) {
                list[target] = transform(list[target] as ChatMessage.Tool)
            }
            state.copy(messages = state.messages + (sessionId to list))
        }
    }

    private suspend fun AppConfig.toMobileConfig(): uniffi.llm_harness_mobile_core.MobileConfig {
        val machine = activeMachine
        return uniffi.llm_harness_mobile_core.MobileConfig(
            gatewayUrl = machine?.gatewayUrl ?: "",
            gatewayToken = machine?.gatewayToken ?: "",
            apiKey = apiKey,
            model = model,
            llmBaseUrl = llmBaseUrl,
            provider = provider,
            allowInsecureHttp = allowInsecureHttp,
            trustProfile = machine?.trustProfile ?: "restricted",
        )
    }

    /**
     * Render the session transcript to a full-length PNG (long screenshot)
     * and return the file. HTML goes through an off-screen WebView.
     */
    fun exportSessionLongScreenshot(sessionId: String, anchor: android.view.View, onDone: (java.io.File?) -> Unit) {
        viewModelScope.launch(Dispatchers.IO) {
            val result = runCatching {
                val history = if (live[sessionId] != null) {
                    live[sessionId]!!.history()
                } else {
                    manager.readHistory(sessionId)
                }
                val sessionName = _uiState.value.sessions
                    .firstOrNull { it.id == sessionId }?.name ?: sessionId
                val html = dev.ohmyharness.mobile.util.TranscriptHtml.build(sessionName, history)
                dev.ohmyharness.mobile.util.LongScreenshot.capture(
                    appContext, anchor, sessionName, html,
                )
            }
            onDone(result.getOrNull())
        }
    }

    /**
     * Export a session transcript to Markdown; returns the written file.
     * Layout: one heading per turn, user lines quoted, assistant text as-is,
     * tool calls as fenced logs.
     */
    fun exportSessionMarkdown(sessionId: String, onDone: (java.io.File?) -> Unit) {
        viewModelScope.launch(Dispatchers.IO) {
            val result = runCatching {
                val history = if (live[sessionId] != null) {
                    live[sessionId]!!.history()
                } else {
                    manager.readHistory(sessionId)
                }
                val sessionName = _uiState.value.sessions
                    .firstOrNull { it.id == sessionId }?.name ?: sessionId
                val md = buildString {
                    appendLine("# ${sessionName.replace('\n', ' ')}")
                    appendLine()
                    appendLine("> Exported from oh-my-harness · ${java.text.SimpleDateFormat("yyyy-MM-dd HH:mm", java.util.Locale.US).format(java.util.Date())}")
                    appendLine()
                    for (m in history) {
                        when (m.role) {
                            "user" -> {
                                appendLine("**User:**")
                                appendLine()
                                appendLine(m.text)
                                appendLine()
                            }
                            "assistant" -> {
                                appendLine("**Assistant:**")
                                appendLine()
                                appendLine(m.text)
                                appendLine()
                            }
                            "tool" -> {
                                appendLine("**Tool** ${if (m.isError) "(error)" else ""}:")
                                appendLine()
                                appendLine("```")
                                appendLine(m.text)
                                appendLine("```")
                                appendLine()
                            }
                            else -> {
                                if (m.text.isNotBlank()) {
                                    appendLine("*${m.text}*")
                                    appendLine()
                                }
                            }
                        }
                    }
                }
                val dir = java.io.File(appContext.cacheDir, "exports").apply { mkdirs() }
                val safe = sessionName.replace(Regex("[^A-Za-z0-9\u4e00-\u9fa5-_ ]"), "_").take(60)
                    .ifBlank { sessionId }
                val file = java.io.File(dir, "$safe.md")
                file.writeText(md.toString())
                file
            }
            onDone(result.getOrNull())
        }
    }

    /** Refresh machine list (presence probing happens in MachinesScreen). */
    fun refreshMachines() {
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { machineRepository.current() }.onSuccess { list ->
                _uiState.update { it.copy(machines = list) }
            }
        }
    }

    fun saveMachine(machine: dev.ohmyharness.mobile.data.Machine) {
        viewModelScope.launch(Dispatchers.IO) {
            machineRepository.upsert(machine)
            if (activeMachine?.id == machine.id) activeMachine = machine
            _uiState.update { it.copy(machines = machineRepository.current()) }
        }
    }

    fun deleteMachine(id: String) {
        viewModelScope.launch(Dispatchers.IO) {
            machineRepository.delete(id)
            if (activeMachine?.id == id) activeMachine = machineRepository.current().firstOrNull()
            _uiState.update { it.copy(machines = machineRepository.current()) }
        }
    }

    private fun refreshSessions() {

        viewModelScope.launch(Dispatchers.IO) {
            runCatching { manager.listSessions() }.onSuccess { list ->
                _uiState.update { it.copy(sessions = list) }
            }
        }
    }

    private fun toChatMessage(m: uniffi.llm_harness_mobile_core.MobileHistoryMessage): ChatMessage =
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

    private fun replayHistory(sessionId: String, session: MobileSession) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val replayed = session.history().map(::toChatMessage)
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
        /** Coalescing window for streaming text deltas (see [appendDelta]). */
        private const val DELTA_FRAME_MS = 150L

        fun factory(
            configRepository: ConfigRepository,
            machineRepository: dev.ohmyharness.mobile.data.MachineRepository,
            llmRepository: dev.ohmyharness.mobile.data.LlmRepository,
            filesDir: java.io.File,
            appContext: android.content.Context,
        ): ViewModelProvider.Factory = object : ViewModelProvider.Factory {
            @Suppress("UNCHECKED_CAST")
            override fun <T : ViewModel> create(modelClass: Class<T>): T =
                AppViewModel(
                    configRepository,
                    machineRepository,
                    llmRepository,
                    filesDir.absolutePath,
                    appContext.applicationContext,
                ) as T
        }
    }
}
