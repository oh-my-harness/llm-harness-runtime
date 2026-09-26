package dev.ohmyharness.mobile

import android.os.Bundle
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.llm_harness_mobile_core.MobileConfig
import uniffi.llm_harness_mobile_core.MobileEvent
import uniffi.llm_harness_mobile_core.connect

/**
 * MVP chat screen: configuration fields + transcript + input. Events are
 * polled from the Rust-side queue on a 100ms timer; text deltas append to the
 * streaming assistant block.
 */
class MainActivity : AppCompatActivity() {
    private var session: uniffi.llm_harness_mobile_core.MobileSession? = null
    private var pollJob: Job? = null
    private val transcript = StringBuilder()
    private var inAssistantBlock = false

    private lateinit var transcriptView: TextView
    private lateinit var scroll: ScrollView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        transcriptView = findViewById(R.id.transcript)
        scroll = findViewById(R.id.scroll)

        findViewById<Button>(R.id.connectBtn).setOnClickListener {
            connectAndStart()
        }
        findViewById<Button>(R.id.sendBtn).setOnClickListener {
            val input = findViewById<EditText>(R.id.input)
            val text = input.text.toString().trim()
            if (text.isNotEmpty() && session != null) {
                appendUser(text)
                session!!.send(text)
                input.setText("")
            }
        }
    }

    private fun connectAndStart() {
        if (session != null) return
        val config = MobileConfig(
            gatewayUrl = findViewById<EditText>(R.id.endpoint).text.toString().trim(),
            gatewayToken = findViewById<EditText>(R.id.token).text.toString().trim(),
            apiKey = findViewById<EditText>(R.id.apiKey).text.toString().trim(),
            model = findViewById<EditText>(R.id.model).text.toString().trim(),
            llmBaseUrl = findViewById<EditText>(R.id.llmBaseUrl).text.toString().trim(),
            provider = "openai",
            allowInsecureHttp = findViewById<android.widget.CheckBox>(R.id.insecureHttp).isChecked,
            trustProfile = when (findViewById<android.widget.RadioGroup>(R.id.trustProfile).checkedRadioButtonId) {
                R.id.profileTrustedLan -> "trusted-lan"
                R.id.profileTrustedFull -> "trusted-full"
                else -> "restricted"
            },
        )
        appendSystem("connecting to ${config.gatewayUrl} …")
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val s = connect(config)
                session = s
                runOnUiThread {
                    appendSystem("connected (trust profile: ${config.trustProfile}). send a message.")
                    findViewById<View>(R.id.connectBtn).isEnabled = false
                }
                startPolling(s)
            } catch (e: Exception) {
                runOnUiThread { appendSystem("connect failed: ${e.message}") }
            }
        }
    }

    private fun startPolling(s: uniffi.llm_harness_mobile_core.MobileSession) {
        pollJob?.cancel()
        pollJob = CoroutineScope(Dispatchers.IO).launch {
            while (isActive) {
                val event = s.pollEvent()
                if (event != null) {
                    runOnUiThread { render(event) }
                } else {
                    delay(100)
                }
            }
        }
    }

    private fun render(event: MobileEvent) {
        when (event) {
            is MobileEvent.TextDelta -> appendAssistantDelta(event.text)
            is MobileEvent.ToolStarted -> appendSystem("… running ${event.name}")
            is MobileEvent.ToolFinished -> appendSystem("· ${event.name}: ${event.summary}")
            is MobileEvent.RunFinished -> {
                inAssistantBlock = false
                appendRaw("\n")
            }
            is MobileEvent.Failed -> appendSystem("error: ${event.message}")
        }
    }

    private fun appendUser(text: String) {
        appendRaw("\n══ you ══\n$text\n")
        inAssistantBlock = false
    }

    private fun appendAssistantDelta(delta: String) {
        if (!inAssistantBlock) {
            appendRaw("\n── agent ──\n")
            inAssistantBlock = true
        }
        appendRaw(delta)
    }

    private fun appendSystem(text: String) {
        appendRaw("\n· $text\n")
    }

    private fun appendRaw(text: String) {
        transcript.append(text)
        transcriptView.text = transcript
        scroll.post { scroll.fullScroll(View.FOCUS_DOWN) }
    }

    override fun onDestroy() {
        pollJob?.cancel()
        session?.close()
        session = null
        super.onDestroy()
    }
}
