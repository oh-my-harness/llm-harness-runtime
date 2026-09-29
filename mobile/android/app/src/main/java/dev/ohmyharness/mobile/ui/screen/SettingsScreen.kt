package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.ui.component.ConfigForm

/** Settings reuses the setup form; saving persists only. */
@Composable
fun SettingsScreen(
    current: dev.ohmyharness.mobile.data.AppConfig,
    onSave: (AppConfig) -> Unit,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    androidx.compose.foundation.layout.Column(
        modifier = modifier.fillMaxSize().padding(16.dp),
    ) {
        TextButton(onClick = onBack) { Text("< Back") }
        ConfigForm(initial = current, onSave = onSave, modifier = Modifier.fillMaxWidth())
    }
}
