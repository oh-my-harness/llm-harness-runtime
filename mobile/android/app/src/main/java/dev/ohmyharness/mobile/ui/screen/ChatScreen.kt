package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ripple
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Photo
import androidx.compose.material.icons.filled.SaveAlt
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.draw.clip
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ConnectionState
import dev.ohmyharness.mobile.ui.ChatMessage
import dev.ohmyharness.mobile.ui.component.ConnectionBanner
import dev.ohmyharness.mobile.ui.theme.JetBrainsMono
import dev.ohmyharness.mobile.ui.theme.OhmTypography
import dev.ohmyharness.mobile.ui.component.MessageBubble
import dev.ohmyharness.mobile.ui.component.ToolStatusCard
import dev.ohmyharness.mobile.ui.theme.OhmRadius
import dev.ohmyharness.mobile.ui.theme.OhmSpacing
import android.widget.Toast
import androidx.compose.ui.platform.LocalContext

/** The chat screen: header row + banner + transcript + input bar. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    state: dev.ohmyharness.mobile.ui.AppUiState,
    inputText: String,
    onInput: (String) -> Unit,
    onSend: (String) -> Unit,
    onStop: () -> Unit,
    onSteer: (String) -> Unit,
    onRetry: () -> Unit,
    onSessions: () -> Unit,
    onSettings: () -> Unit,
    onExport: () -> Unit = {},
    onExportPng: () -> Unit = {},
    toolDisplay: dev.ohmyharness.mobile.ui.ToolDisplay = dev.ohmyharness.mobile.ui.ToolDisplay.Tail,
    modifier: Modifier = Modifier,
) {
    val activeId = state.activeSessionId
    val allMessages = state.messages[activeId].orEmpty()
    var searchQuery by remember { mutableStateOf("") }
    var searchOpen by remember { mutableStateOf(false) }
    val messages = if (searchQuery.isBlank()) allMessages else allMessages.filter { m ->
        when (m) {
            is ChatMessage.User -> m.text.contains(searchQuery, ignoreCase = true)
            is ChatMessage.Assistant -> m.text.contains(searchQuery, ignoreCase = true)
            is ChatMessage.Tool ->
                (m.summary + " " + (m.fullOutput ?: "")).contains(searchQuery, ignoreCase = true)
            is ChatMessage.SystemNotice -> m.text.contains(searchQuery, ignoreCase = true)
        }
    }
    Column(modifier = modifier.fillMaxWidth()) {
        TopAppBar(
            title = {
                Text(
                    "oh-my-harness",
                    style = OhmTypography.titleMedium.copy(fontFamily = JetBrainsMono),
                )
            },
            actions = {
                IconButton(onClick = {
                    if (searchOpen) searchQuery = ""
                    searchOpen = !searchOpen
                }) {
                    Icon(
                        imageVector = if (searchOpen) Icons.Filled.Close else Icons.Filled.Search,
                        contentDescription = "Search",
                    )
                }
                IconButton(onClick = onSessions) {
                    Icon(Icons.AutoMirrored.Filled.List, contentDescription = "Sessions")
                }
                IconButton(onClick = onExport) {
                    Icon(Icons.Filled.SaveAlt, contentDescription = "Export")
                }
                IconButton(onClick = onExportPng) {
                    Icon(Icons.Filled.Photo, contentDescription = "Long screenshot")
                }
                IconButton(onClick = onSettings) {
                    Icon(Icons.Filled.Settings, contentDescription = "Settings")
                }
            },
            colors = TopAppBarDefaults.topAppBarColors(
                containerColor = MaterialTheme.colorScheme.surface,
            ),
        )

        if (searchOpen) {
            OutlinedTextField(
                value = searchQuery,
                onValueChange = { searchQuery = it },
                modifier = Modifier.fillMaxWidth().padding(horizontal = OhmSpacing.sm),
                singleLine = true,
                placeholder = { Text("Search in session") },
                textStyle = MaterialTheme.typography.bodySmall,
            )
        }

        ConnectionBanner(state = state.connection, onRetry = onRetry)

        MessageList(
            messages = messages,
            toolDisplay = toolDisplay,
            modifier = Modifier.weight(1f),
        )

        InputBar(
            enabled = activeId != null && state.connection is ConnectionState.Connected,
            value = inputText,
            onValueChange = onInput,
            onSend = onSend,
            onStop = onStop,
            onSteer = onSteer,
            streaming = (messages.lastOrNull() is ChatMessage.Assistant &&
                (messages.lastOrNull() as ChatMessage.Assistant).isStreaming) ||
                messages.any { it is ChatMessage.Tool && it.running },
        )
    }
}

@Composable
private fun MessageList(
    messages: List<ChatMessage>,
    toolDisplay: dev.ohmyharness.mobile.ui.ToolDisplay,
    modifier: Modifier = Modifier,
) {
    val listState = rememberLazyListState()
    LaunchedEffect(messages.size) {
        if (messages.isNotEmpty()) listState.animateScrollToItem(messages.lastIndex)
    }
    LazyColumn(state = listState, modifier = modifier.fillMaxWidth()) {
        items(messages, key = { it.key }) { message ->
            Box(modifier = Modifier.fillMaxWidth().padding(horizontal = OhmSpacing.sm, vertical = OhmSpacing.messageGap / 2)) {
                when (message) {
                    is ChatMessage.User -> UserBubble(message)
                    is ChatMessage.Assistant -> Column { MessageBubble(message) }
                    is ChatMessage.Tool -> if (toolDisplay != dev.ohmyharness.mobile.ui.ToolDisplay.Hidden) {
                        ToolStatusCard(message, display = toolDisplay)
                    } // Hidden: skip entirely
                    is ChatMessage.SystemNotice -> SystemNotice(message)
                }
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun UserBubble(message: ChatMessage.User, modifier: Modifier = Modifier) {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    val interaction = remember { MutableInteractionSource() }
    Column(horizontalAlignment = Alignment.End) {
        Text(
            text = message.text,
            modifier = Modifier
                .widthIn(max = 300.dp)
                .clip(RoundedCornerShape(OhmRadius.bubble))
                .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.15f))
                .combinedClickable(
                    interactionSource = interaction,
                    indication = ripple(),
                    onClick = {},
                    onLongClick = {
                        clipboard.setText(AnnotatedString(message.text))
                        Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
                    },
                )
                .padding(horizontal = OhmSpacing.md, vertical = OhmSpacing.sm),
            color = MaterialTheme.colorScheme.onBackground,
            style = MaterialTheme.typography.bodyMedium,
        )
        if (message.queued) {
            Text(
                "· queued",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = OhmSpacing.sm, vertical = OhmSpacing.xs),
            )
        }
    }
}

@Composable
private fun SystemNotice(message: ChatMessage.SystemNotice, modifier: Modifier = Modifier) {
    Text(
        text = "· ${message.text}",
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = modifier.fillMaxWidth(),
    )
}

