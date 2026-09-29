package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ConnectionState

/**
 * Top connection banner; hidden while connected. Failed shows the full error
 * text (verbatim per the spec's no-swallowing rule) with a retry button.
 */
@Composable
fun ConnectionBanner(
    state: ConnectionState,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
) {
    when (state) {
        is ConnectionState.Failed -> Surface(
            color = MaterialTheme.colorScheme.errorContainer,
            modifier = modifier.fillMaxWidth(),
        ) {
            androidx.compose.foundation.layout.Row(
                modifier = Modifier.padding(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = state.message,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onErrorContainer,
                    modifier = Modifier.weight(1f),
                )
                TextButton(onClick = onRetry) { Text("Retry") }
            }
        }

        ConnectionState.Connecting -> Surface(modifier = modifier.fillMaxWidth()) {
            androidx.compose.foundation.layout.Column(
                modifier = Modifier.padding(8.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                Text("connecting…", style = MaterialTheme.typography.bodySmall)
            }
        }

        else -> {}
    }
}
