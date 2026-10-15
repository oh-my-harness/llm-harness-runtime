package dev.ohmyharness.mobile.ui.nav

import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.compose.runtime.collectAsState
import androidx.navigation.compose.rememberNavController
import dev.ohmyharness.mobile.ui.AppViewModel
import dev.ohmyharness.mobile.ui.screen.ChatScreen
import dev.ohmyharness.mobile.ui.screen.SettingsScreen
import dev.ohmyharness.mobile.ui.screen.SetupScreen
import dev.ohmyharness.mobile.ui.screen.SessionsScreen

object Routes {
    const val MACHINES = "machines"
    const val TASKS = "tasks/{machineId}"
    const val CHAT = "chat"
    const val SETUP = "setup"
    const val SETTINGS = "settings"
    const val SESSIONS = "sessions"

    fun tasks(machineId: String?) = "tasks/$machineId"
    const val CHAT_ARG = "chat/{sessionId}"
}

/** App navigation graph; start destination gated on saved config. */
@Composable
fun AppNavHost(viewModel: AppViewModel, startOnSetup: Boolean, modifier: Modifier = Modifier) {
    val navController = rememberNavController()
    val state = viewModel.uiState.collectAsState().value
    var input by rememberSaveable { mutableStateOf("") }
    val startDestination = if (startOnSetup) Routes.SETUP else Routes.MACHINES

    NavHost(navController = navController, startDestination = startDestination, modifier = modifier) {
        composable(Routes.SETUP) {
            SetupScreen(
                current = state.config,
                onSave = { config ->
                    viewModel.saveConfig(config)
                    navController.navigate(Routes.MACHINES) {
                        popUpTo(Routes.SETUP) { inclusive = true }
                    }
                },
            )
        }
        composable(Routes.MACHINES) {
            dev.ohmyharness.mobile.ui.screen.MachinesScreen(
                machines = state.machines,
                runningCount = { machineId -> 0 }, // wiring lands with task status (Batch 3b)
                onSelect = { machine ->
                    navController.navigate("tasks/${machine.id}")
                },
                onCreate = { navController.navigate(Routes.SETTINGS) },
                onDelete = { viewModel.deleteMachine(it) },
            )
        }
        composable(Routes.TASKS) { backStackEntry ->
            val machineId = backStackEntry.arguments?.getString("machineId")
            // All sessions belong to the entered machine for now; per-task
            // machine binding lands with the SessionState extension.
            SessionsScreen(
                sessions = state.sessions,
                activeSessionId = state.activeSessionId,
                unread = state.unread,
                onSelect = { id ->
                    viewModel.switchSession(id)
                    navController.navigate(Routes.CHAT)
                },
                onCreate = {
                    viewModel.newSession("Task ${state.sessions.size + 1}")
                    navController.navigate(Routes.CHAT)
                },
                onDelete = { id -> viewModel.deleteSession(id) },
                onBack = { navController.popBackStack() },
            )
        }
        composable(Routes.CHAT) {
            ChatScreen(
                state = state,
                inputText = input,
                onInput = { input = it },
                onSend = { text ->
                    viewModel.send(text)
                    input = ""
                },
                onStop = viewModel::stop,
                onSteer = { text ->
                    viewModel.steer(text)
                    input = ""
                },
                onRetry = viewModel::retry,
                onSessions = { navController.navigate(Routes.SESSIONS) },
                onSettings = { navController.navigate(Routes.SETTINGS) },
                onExport = {
                    val context = navController.context
                    val sessionId = state.activeSessionId
                    if (sessionId != null) {
                        viewModel.exportSessionMarkdown(sessionId) { file ->
                            if (file != null) {
                                val where = dev.ohmyharness.mobile.util.DownloadsSaver.save(
                                    context, file, "text/markdown",
                                )
                                android.widget.Toast.makeText(
                                    context,
                                    where ?: "Save failed",
                                    android.widget.Toast.LENGTH_LONG,
                                ).show()
                            }
                        }
                    }
                },
                toolDisplay = state.toolDisplay,
                onExportPng = {
                    val context = navController.context
                    val sessionId = state.activeSessionId
                    val anchor = (context as? android.app.Activity)?.window?.decorView
                    if (sessionId != null && anchor != null) {
                        viewModel.exportSessionLongScreenshot(sessionId, anchor) { file ->
                            if (file != null) {
                                val where = dev.ohmyharness.mobile.util.DownloadsSaver.save(
                                    context, file, "image/png",
                                )
                                android.widget.Toast.makeText(
                                    context,
                                    where ?: "Save failed",
                                    android.widget.Toast.LENGTH_LONG,
                                ).show()
                            }
                        }
                    }
                },
            )
        }
        composable(Routes.SETTINGS) {
            SettingsScreen(
                current = state.config,
                onSave = { config ->
                    viewModel.saveConfig(config)
                    navController.popBackStack()
                },
                onBack = { navController.popBackStack() },
            )
        }
        composable(Routes.SESSIONS) {
            SessionsScreen(
                sessions = state.sessions,
                activeSessionId = state.activeSessionId,
                unread = state.unread,
                onSelect = { id ->
                    viewModel.switchSession(id)
                    navController.popBackStack()
                },
                onCreate = {
                    viewModel.newSession("Session ${state.sessions.size + 1}")
                    navController.popBackStack()
                },
                onDelete = { id -> viewModel.deleteSession(id) },
                onBack = { navController.popBackStack() },
            )
        }
    }
}

