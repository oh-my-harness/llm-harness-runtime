package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.data.AppConfig
import dev.ohmyharness.mobile.ui.component.ConfigForm
import dev.ohmyharness.mobile.ui.theme.OhmTypography

/** Settings reuses the setup form; saving persists only. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(
    current: dev.ohmyharness.mobile.data.AppConfig,
    onSave: (AppConfig) -> Unit,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(modifier = modifier.fillMaxSize()) {
        TopAppBar(
            title = { Text("Settings", style = OhmTypography.titleMedium) },
            navigationIcon = {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
            },
            colors = TopAppBarDefaults.topAppBarColors(
                containerColor = MaterialTheme.colorScheme.surface,
            ),
        )
        // Type scale: applied immediately (theme reads it via config flow)
        // and persisted together with the connection settings.
        var fontScale by remember { mutableFloatStateOf(current.fontScale) }
        var toolDisplay by remember { mutableStateOf(current.toolDisplay) }
        Text(
            "Font size",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp),
        )
        SingleChoiceSegmentedButtonRow(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
        ) {
            FONT_SCALES.forEachIndexed { index, (label, value) ->
                SegmentedButton(
                    selected = fontScale == value,
                    onClick = { fontScale = value },
                    shape = SegmentedButtonDefaults.itemShape(index = index, count = FONT_SCALES.size),
                ) { Text(label) }
            }
        }
        Text(
            "Tool output",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp),
        )
        SingleChoiceSegmentedButtonRow(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
        ) {
            TOOL_DISPLAYS.forEachIndexed { index, (label, value) ->
                SegmentedButton(
                    selected = toolDisplay == value,
                    onClick = { toolDisplay = value },
                    shape = SegmentedButtonDefaults.itemShape(index = index, count = TOOL_DISPLAYS.size),
                ) { Text(label) }
            }
        }
        ConfigForm(
            initial = current,
            onSave = { config ->
                onSave(
                    config.copy(
                        fontScale = fontScale,
                        toolDisplay = toolDisplay,
                    ),
                )
            },
            modifier = Modifier.fillMaxWidth().padding(16.dp),
        )
    }
}

private val TOOL_DISPLAYS = listOf(
    "Hidden" to "hidden",
    "Summary" to "summary",
    "Live tail" to "tail",
)

private val FONT_SCALES = listOf(
    "Small" to 0.85f,
    "Normal" to 1.0f,
    "Large" to 1.15f,
    "X-Large" to 1.3f,
)
