package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.theme.OhmRadius
import dev.ohmyharness.mobile.ui.theme.OhmSpacing

/**
 * Floating rounded input bar. The action button is send; while streaming it
 * becomes stop, and a small steer affordance sits above the bar.
 */
@Composable
fun InputBar(
    enabled: Boolean,
    value: String,
    onValueChange: (String) -> Unit,
    onSend: (String) -> Unit,
    onStop: () -> Unit = {},
    onSteer: (String) -> Unit = {},
    streaming: Boolean = false,
    modifier: Modifier = Modifier,
) {
    val canAct = enabled && value.isNotBlank()
    androidx.compose.foundation.layout.Column(
        modifier = modifier.fillMaxWidth().padding(horizontal = OhmSpacing.sm),
    ) {
        if (streaming && value.isNotBlank()) {
            TextButton(
                onClick = { onSteer(value.trim()) },
                enabled = enabled,
                modifier = Modifier.align(Alignment.End).padding(bottom = OhmSpacing.xs),
            ) { Text("Steer", style = MaterialTheme.typography.labelSmall) }
        }
        Surface(
            shape = RoundedCornerShape(OhmRadius.input),
            color = MaterialTheme.colorScheme.surfaceVariant,
            border = BorderStroke(
                1.dp,
                MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
            ),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Row(
                modifier = Modifier.padding(start = OhmSpacing.md, top = OhmSpacing.xs, bottom = OhmSpacing.xs, end = OhmSpacing.xs),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                androidx.compose.foundation.text.BasicTextField(
                    value = value,
                    onValueChange = onValueChange,
                    enabled = enabled,
                    modifier = Modifier.weight(1f).padding(vertical = OhmSpacing.sm),
                    textStyle = MaterialTheme.typography.bodyMedium.copy(
                        color = MaterialTheme.colorScheme.onBackground,
                    ),
                    maxLines = 5,
                    decorationBox = { inner ->
                        if (value.isEmpty()) {
                            Text(
                                "message",
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                style = MaterialTheme.typography.bodyMedium,
                            )
                        }
                        inner()
                    },
                )
                Surface(
                    shape = CircleShape,
                    color = if (canAct || streaming) {
                        MaterialTheme.colorScheme.primary
                    } else {
                        MaterialTheme.colorScheme.surface
                    },
                    modifier = Modifier.padding(OhmSpacing.xs),
                ) {
                    IconButton(
                        onClick = {
                            when {
                                streaming -> onStop()
                                value.isNotBlank() -> onSend(value.trim())
                            }
                        },
                        enabled = enabled && (streaming || value.isNotBlank()),
                    ) {
                        Icon(
                            if (streaming) Icons.Filled.Stop else Icons.AutoMirrored.Filled.Send,
                            contentDescription = if (streaming) "stop" else "Send",
                            tint = if (canAct || streaming) {
                                MaterialTheme.colorScheme.onPrimary
                            } else {
                                MaterialTheme.colorScheme.onSurfaceVariant
                            },
                        )
                    }
                }
            }
        }
    }
}
