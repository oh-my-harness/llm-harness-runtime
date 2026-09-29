package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.data.isComplete
import dev.ohmyharness.mobile.ui.component.ConfigForm
import dev.ohmyharness.mobile.ui.component.TrustProfileRow

/** First-run configuration. On save the ViewModel connects automatically. */
@Composable
fun SetupScreen(
    current: AppConfig,
    onSave: (AppConfig) -> Unit,
    modifier: Modifier = Modifier,
) {
    androidx.compose.material3.Scaffold(modifier = modifier.fillMaxSize()) { padding ->
        androidx.compose.foundation.layout.Column(
            modifier = Modifier
                .padding(padding)
                .fillMaxSize()
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                "Welcome to Harness Agent",
                style = MaterialTheme.typography.headlineSmall,
            )
            Text(
                "Point the app at a sandbox gateway on your computer to begin.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            ConfigForm(initial = current, onSave = onSave, modifier = Modifier.fillMaxWidth())
        }
    }
}
