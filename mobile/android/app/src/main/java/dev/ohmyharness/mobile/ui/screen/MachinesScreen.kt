package dev.ohmyharness.mobile.ui.screen

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Computer
import androidx.compose.material.icons.filled.Forum
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
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
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.data.Machine
import dev.ohmyharness.mobile.ui.theme.OhmTypography
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

private val PresenceOnline = Color(0xFF3FB950)
private val PresenceOffline = Color(0xFFF85149)
private val PresenceLocal = Color(0xFF9AA4B2)

/** First layer: registered machines. Tap to open the machine's task list. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MachinesScreen(
    machines: List<Machine>,
    runningCount: (String) -> Int,
    onSelect: (Machine) -> Unit,
    onCreate: () -> Unit,
    onDelete: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    Scaffold(
        modifier = modifier.fillMaxSize(),
        topBar = {
            TopAppBar(
                title = { Text("Machines", style = dev.ohmyharness.mobile.ui.theme.OhmTypography.titleMedium) },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
        floatingActionButton = {
            FloatingActionButton(onClick = onCreate) {
                Icon(Icons.Filled.Computer, contentDescription = "new machine")
            }
        },
    ) { padding ->
        if (machines.isEmpty()) {
            Column(
                modifier = Modifier.padding(padding).fillMaxSize().padding(16.dp),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(
                    "No machines yet.\nTap + to add your computer.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            LazyColumn(modifier = Modifier.padding(padding).fillMaxSize()) {
                items(machines, key = { it.id }) { machine ->
                    MachineRow(
                        machine = machine,
                        runningCount = runningCount(machine.id),
                        onClick = { onSelect(machine) },
                        onDelete = { onDelete(machine.id) },
                    )
                }
            }
        }
    }
}

@Composable
private fun MachineRow(
    machine: Machine,
    runningCount: Int,
    onClick: () -> Unit,
    onDelete: () -> Unit,
    modifier: Modifier = Modifier,
) {
    var online by remember(machine.id) { mutableStateOf<Boolean?>(null) }
    LaunchedEffect(machine.id, machine.gatewayUrl) {
        if (machine.gatewayUrl.isBlank()) return@LaunchedEffect
        while (true) {
            online = probeHealthz(machine.gatewayUrl)
            delay(30_000)
        }
    }
    Card(onClick = onClick, modifier = modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp)) {
        Row(modifier = Modifier.padding(12.dp).fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Icon(
                if (machine.gatewayUrl.isBlank()) Icons.Filled.Forum else Icons.Filled.Computer,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.size(22.dp),
            )
            Column(modifier = Modifier.weight(1f).padding(horizontal = 10.dp)) {
                Text(machine.name, style = MaterialTheme.typography.titleSmall)
                Text(
                    when {
                        machine.gatewayUrl.isBlank() -> "chat only · this phone"
                        online == true -> "online"
                        online == false -> "offline"
                        else -> "checking…"
                    },
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (runningCount > 0) {
                Text(
                    "$runningCount running",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            IconButton(onClick = onDelete) {
                Icon(Icons.Filled.Close, contentDescription = "delete machine")
            }
        }
    }
}

private suspend fun probeHealthz(gatewayUrl: String): Boolean = withContext(Dispatchers.IO) {
    if (gatewayUrl.isBlank()) return@withContext true
    runCatching {
        val conn = java.net.URL(gatewayUrl.trimEnd('/') + "/healthz").openConnection()
            as java.net.HttpURLConnection
        conn.connectTimeout = 2_000
        conn.readTimeout = 2_000
        try {
            conn.responseCode in 200..299
        } finally {
            conn.disconnect()
        }
    }.getOrDefault(false)
}
