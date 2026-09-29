package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
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
    val gatewayUrl: String = "",
    val gatewayToken: String = "",
    val apiKey: String = "",
    val model: String = "",
)

/**
 * Shared configuration form for the setup wizard and settings. Secrets get
 * visibility toggles; the LLM base URL and network switch live under Advanced.
 */
@Composable
fun ConfigForm(
    initial: AppConfig,
    onSave: (AppConfig) -> Unit,
    modifier: Modifier = Modifier,
) {
    var gatewayUrl by rememberSaveable { mutableStateOf(initial.gatewayUrl) }
    var gatewayToken by rememberSaveable { mutableStateOf(initial.gatewayToken) }
    var apiKey by rememberSaveable { mutableStateOf(initial.apiKey) }
    var model by rememberSaveable { mutableStateOf(initial.model) }
    var llmBaseUrl by rememberSaveable { mutableStateOf(initial.llmBaseUrl) }
    var allowInsecure by rememberSaveable { mutableStateOf(initial.allowInsecureHttp) }
    var trustProfile by rememberSaveable { mutableStateOf(initial.trustProfile) }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var submitted by rememberSaveable { mutableStateOf(false) }

    val draft = AppConfig(
        gatewayUrl = gatewayUrl.trim(),
        gatewayToken = gatewayToken.trim(),
        apiKey = apiKey,
        model = model.trim(),
        llmBaseUrl = llmBaseUrl.trim(),
        provider = "openai",
        allowInsecureHttp = allowInsecure,
        trustProfile = trustProfile,
    )
    val errors = if (submitted) validateDraft(draft) else ConfigErrors()

    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedField(
            label = "Gateway URL",
            value = gatewayUrl,
            onValueChange = { gatewayUrl = it },
            error = errors.gatewayUrl,
            keyboardType = KeyboardType.Uri,
        )
        SecretField(
            label = "Gateway token",
            value = gatewayToken,
            onValueChange = { gatewayToken = it },
            error = errors.gatewayToken,
        )
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

        Text("Trust profile", style = MaterialTheme.typography.labelMedium)
        TrustProfileRow(selected = trustProfile, onSelect = { trustProfile = it })

        androidx.compose.material3.Button(
            onClick = {
                submitted = true
                if (validateDraft(draft) == ConfigErrors()) onSave(draft)
            },
            modifier = modifier.fillMaxWidth(),
        ) { Text("Save & connect") }
    }
}

internal fun validateDraft(config: AppConfig): ConfigErrors = ConfigErrors(
    gatewayUrl =
        when {
            config.gatewayUrl.isBlank() -> "gateway URL is required"
            !config.gatewayUrl.startsWith("http://") &&
                !config.gatewayUrl.startsWith("https://") ->
                "must start with http:// or https://"
            else -> ""
        },
    gatewayToken = if (config.gatewayToken.isBlank()) "gateway token is required" else "",
    apiKey = if (config.apiKey.isBlank()) "API key is required" else "",
    model = if (config.model.isBlank()) "model is required" else "",
)
