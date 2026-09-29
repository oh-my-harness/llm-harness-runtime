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
    const val CHAT = "chat"
    const val SETUP = "setup"
    const val SETTINGS = "settings"
    const val SESSIONS = "sessions"
}

/** App navigation graph; start destination gated on saved config. */
@Composable
fun AppNavHost(viewModel: AppViewModel, startOnSetup: Boolean, modifier: Modifier = Modifier) {
    val navController = rememberNavController()
    val state = viewModel.uiState.collectAsState().value
    var input by rememberSaveable { mutableStateOf("") }
    val startDestination = if (startOnSetup) Routes.SETUP else Routes.CHAT

    NavHost(navController = navController, startDestination = startDestination, modifier = modifier) {
        composable(Routes.SETUP) {
            SetupScreen(
                current = state.config,
                onSave = { config ->
                    viewModel.saveConfig(config)
                    navController.navigate(Routes.CHAT) {
                        popUpTo(Routes.SETUP) { inclusive = true }
                    }
                },
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
                onRetry = viewModel::retry,
                onSessions = { navController.navigate(Routes.SESSIONS) },
                onSettings = { navController.navigate(Routes.SETTINGS) },
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
