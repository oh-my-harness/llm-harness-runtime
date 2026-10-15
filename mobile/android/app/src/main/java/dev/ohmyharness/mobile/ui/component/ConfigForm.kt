package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.data.AppConfig

/** Field-level validation errors; empty string means valid. */
data class ConfigErrors(
    val apiKey: String = "",
    val model: String = "",
)

/**
 * LLM configuration form (setup wizard and settings). Machine fields moved
 * to the machine editor (Batch 3).
 */
@Composable
fun ConfigForm(
    initial: AppConfig,
    onSave: (AppConfig) -> Unit,
    modifier: Modifier = Modifier,
) {
    var apiKey by rememberSaveable { mutableStateOf(initial.apiKey) }
    var model by rememberSaveable { mutableStateOf(initial.model) }
    var llmBaseUrl by rememberSaveable { mutableStateOf(initial.llmBaseUrl) }
    var allowInsecure by rememberSaveable { mutableStateOf(initial.allowInsecureHttp) }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var submitted by rememberSaveable { mutableStateOf(false) }

    val draft = AppConfig(
        apiKey = apiKey,
        model = model.trim(),
        llmBaseUrl = llmBaseUrl.trim(),
        provider = "openai",
        allowInsecureHttp = allowInsecure,
    )
    val errors = if (submitted) validateDraft(draft) else ConfigErrors()

    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        SecretField(
            label = "LLM API key",
            value = apiKey,
            onValueChange = { apiKey = it },
            error = errors.apiKey,
        )
        OutlinedField(
            label = "Model",
            value = model,
            onValueChange = { model = it },
            error = errors.model,
        )

        TextButton(onClick = { advanced = !advanced }) {
            Text(if (advanced) "Hide advanced" else "Advanced")
        }
        if (advanced) {
            OutlinedField(
                label = "LLM base URL (optional)",
                value = llmBaseUrl,
                onValueChange = { llmBaseUrl = it },
                error = "",
                modifier = Modifier,
            )
        }

        InsecureHttpRow(checked = allowInsecure, onChange = { allowInsecure = it })

        Button(
            onClick = {
                submitted = true
                if (validateDraft(draft) == ConfigErrors()) onSave(draft)
            },
            modifier = modifier.fillMaxWidth(),
        ) { Text("Save") }
    }
}

internal fun validateDraft(config: AppConfig): ConfigErrors = ConfigErrors(
    apiKey = if (config.apiKey.isBlank()) "API key is required" else "",
    model = if (config.model.isBlank()) "model is required" else "",
)
