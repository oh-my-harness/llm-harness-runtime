package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.TextButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.draw.clip
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ConnectionState
import dev.ohmyharness.mobile.ui.ChatMessage
import dev.ohmyharness.mobile.ui.component.ConnectionBanner
import dev.ohmyharness.mobile.ui.component.MessageBubble
import dev.ohmyharness.mobile.ui.component.ToolStatusCard

/** The chat screen: header row + banner + transcript + input bar. */
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
    modifier: Modifier = Modifier,
) {
    val activeId = state.activeSessionId
    val messages = state.messages[activeId].orEmpty()
    Column(modifier = modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 2.dp),
        ) {
            TextButton(onClick = onSessions) { Text("Sessions") }
            Box(modifier = Modifier.weight(1f), contentAlignment = Alignment.Center) {
                Text(
                    connectionLabel(state.connection),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            TextButton(onClick = onSettings) { Text("Settings") }
        }

        ConnectionBanner(state = state.connection, onRetry = onRetry)

        MessageList(messages = messages, modifier = Modifier.weight(1f))

        InputBar(
            enabled = activeId != null && state.connection is ConnectionState.Connected,
            value = inputText,
            onValueChange = onInput,
            onSend = onSend,
            onStop = onStop,
            onSteer = onSteer,
            streaming = messages.lastOrNull() is ChatMessage.Assistant &&
                (messages.lastOrNull() as ChatMessage.Assistant).isStreaming,
        )
    }
}

@Composable
private fun MessageList(messages: List<ChatMessage>, modifier: Modifier = Modifier) {
    val listState = rememberLazyListState()
    LaunchedEffect(messages.size) {
        if (messages.isNotEmpty()) listState.animateScrollToItem(messages.lastIndex)
    }
    LazyColumn(state = listState, modifier = modifier.fillMaxWidth()) {
        items(messages.size) { index ->
            val message = messages[index]
            Box(modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp)) {
                when (message) {
                    is ChatMessage.User -> UserBubble(message)
                    is ChatMessage.Assistant -> Column { MessageBubble(message) }
                    is ChatMessage.Tool -> ToolStatusCard(message)
                    is ChatMessage.SystemNotice -> SystemNotice(message)
                }
            }
        }
    }
}

@Composable
private fun UserBubble(message: ChatMessage.User, modifier: Modifier = Modifier) {
    Row(modifier = modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
        Text(
            text = message.text,
            modifier = Modifier
                .widthIn(max = 300.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(MaterialTheme.colorScheme.primary)
                .padding(10.dp),
            color = MaterialTheme.colorScheme.onPrimary,
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}

@Composable
private fun SystemNotice(message: ChatMessage.SystemNotice, modifier: Modifier = Modifier) {
    Text(
        text = "· ${message.text}",
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.tertiary,
        modifier = modifier.fillMaxWidth(),
    )
}

private fun connectionLabel(state: ConnectionState): String = when (state) {
    is ConnectionState.Connected -> "connected (${state.trustProfile})"
    ConnectionState.Connecting -> "connecting…"
    is ConnectionState.Failed -> "connection failed"
    ConnectionState.Idle -> "not connected"
}
