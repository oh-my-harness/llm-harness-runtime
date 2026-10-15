package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ChatMessage

/**
 * Tool execution status card. Red left border marks errors; tap toggles the
 * full output (when present).
 */
@Composable
fun ToolStatusCard(
    tool: ChatMessage.Tool,
    display: dev.ohmyharness.mobile.ui.ToolDisplay = dev.ohmyharness.mobile.ui.ToolDisplay.Tail,
    modifier: Modifier = Modifier,
) {
    val expanded = remember(tool.key) { androidx.compose.runtime.mutableStateOf(false) }
    val borderColor =
        if (tool.isError) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary
    Box(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(6.dp))
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .clickable(enabled = tool.fullOutput != null) { expanded.value = !expanded.value },
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .width(3.dp)
                .background(borderColor),
        )
        Column(modifier = Modifier.padding(8.dp)) {
            if (tool.running) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(14.dp),
                        strokeWidth = 2.dp,
                    )
                    Spacer(Modifier.width(6.dp))
                    ElapsedText(startedAtMs = tool.startedAtMs)
                }
            }
            // Live streaming tail (Tail display mode): last ~8 output lines.
            if (display == dev.ohmyharness.mobile.ui.ToolDisplay.Tail &&
                tool.running && !tool.liveTail.isNullOrBlank()
            ) {
                Text(
                    text = tool.liveTail!!,
                    fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            Text(
                text = buildString {
                append("🔧 ${tool.name}: ${tool.summary}")
                if (tool.fullOutput != null) append(if (expanded.value) " ▾" else " ▸")
            },
                style = MaterialTheme.typography.bodySmall,
                color = if (tool.isError) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurface
                },
            )
            if (expanded.value && tool.fullOutput != null) {
                DiffText(output = tool.fullOutput!!, modifier = Modifier.padding(top = 4.dp))
            }
        }
    }
}

@Composable
private fun ElapsedText(startedAtMs: Long) {
    var nowMs by remember { mutableLongStateOf(startedAtMs) }
    LaunchedEffect(Unit) {
        while (true) {
            kotlinx.coroutines.delay(1000)
            nowMs = System.currentTimeMillis()
        }
    }
    Text(
        text = "${((nowMs - startedAtMs).coerceAtLeast(0)) / 1000}s",
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.primary,
    )
}


/** Tool output with +/- diff line coloring when it looks like a diff. */
@Composable
private fun DiffText(output: String, modifier: Modifier = Modifier) {
    val isDiff = output.startsWith("diff --git") || output.lines().any { it.startsWith("@@") }
    if (!isDiff) {
        Text(
            text = output,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            modifier = modifier,
        )
        return
    }
    Column(modifier = modifier) {
        output.lines().forEach { line ->
            val (bg, fg) = when {
                line.startsWith("+") && !line.startsWith("+++") ->
                    Color(0xFF2EA043).copy(alpha = 0.15f) to Color(0xFF3FB950)
                line.startsWith("-") && !line.startsWith("---") ->
                    Color(0xFFF85149).copy(alpha = 0.15f) to Color(0xFFF85149)
                line.startsWith("@@") -> Color(0xFF4EC9B0).copy(alpha = 0.15f) to MaterialTheme.colorScheme.onSurface
                else -> Color.Transparent to MaterialTheme.colorScheme.onSurface
            }
            Text(
                text = line.ifEmpty { " " },
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
                color = fg,
                modifier = Modifier.background(bg).fillMaxWidth(),
            )
        }
    }
}
