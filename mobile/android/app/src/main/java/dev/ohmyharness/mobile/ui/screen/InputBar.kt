package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Bottom input bar: message field + voice placeholder (disabled Add icon) +
 * send/stop. Stop appears while the assistant is streaming.
 */
@Composable
fun InputBar(
    enabled: Boolean,
    value: String,
    onValueChange: (String) -> Unit,
    onSend: (String) -> Unit,
    onStop: () -> Unit = {},
    streaming: Boolean = false,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier = modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OutlinedTextField(
            value = value,
            onValueChange = onValueChange,
            modifier = Modifier.weight(1f),
            enabled = enabled,
            placeholder = { Text("message") },
            maxLines = 4,
        )
        IconButton(onClick = {}, enabled = false) {
            Icon(Icons.Filled.Add, contentDescription = "voice (coming soon)")
        }
        if (streaming) {
            IconButton(onClick = onStop, enabled = enabled) {
                Icon(Icons.Filled.Close, contentDescription = "stop")
            }
        } else {
            IconButton(
                onClick = { if (value.isNotBlank()) onSend(value.trim()) },
                enabled = enabled && value.isNotBlank(),
            ) {
                Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "Send")
            }
        }
    }
}