package dev.ohmyharness.mobile.ui.component

import android.widget.Toast
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.model.rememberMarkdownState
import dev.ohmyharness.mobile.ui.ChatMessage
import dev.ohmyharness.mobile.ui.theme.CodeTextStyle
import dev.ohmyharness.mobile.ui.theme.OhmSpacing

/**
 * Assistant markdown message. Streaming deltas re-parse synchronously with
 * the previous frame retained (no flicker); finished messages parse async.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun MessageBubble(message: ChatMessage.Assistant, modifier: Modifier = Modifier) {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    val interaction = remember { MutableInteractionSource() }
    Column(
        modifier = modifier
            .fillMaxWidth()
            .combinedClickable(
                interactionSource = interaction,
                indication = ripple(),
                onClick = {},
                onLongClick = {
                    clipboard.setText(AnnotatedString(message.text))
                    Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
                },
            )
            .padding(vertical = OhmSpacing.sm),
    ) {
        Markdown(
            markdownState = rememberMarkdownState(
                content = message.text,
                retainState = true,
                immediate = message.isStreaming,
            ),
            typography = com.mikepenz.markdown.m3.markdownTypography(
                h1 = MaterialTheme.typography.titleMedium,
                h2 = MaterialTheme.typography.titleMedium,
                h3 = MaterialTheme.typography.titleMedium,
                text = MaterialTheme.typography.bodyMedium,
                paragraph = MaterialTheme.typography.bodyMedium,
                list = MaterialTheme.typography.bodyMedium,
                code = CodeTextStyle.copy(color = MaterialTheme.colorScheme.onSurface),
            ),
        )
        if (message.isStreaming) {
            Text(text = "…", color = MaterialTheme.colorScheme.primary)
        }
    }
}
