package dev.ohmyharness.mobile

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.compose.runtime.getValue
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.ohmyharness.mobile.data.ConfigRepository
import dev.ohmyharness.mobile.data.isComplete
import dev.ohmyharness.mobile.ui.AppViewModel
import dev.ohmyharness.mobile.ui.nav.AppNavHost
import dev.ohmyharness.mobile.ui.theme.OhmMobileTheme
import kotlinx.coroutines.launch

/**
 * Compose host: wires the ViewModel with DataStore-backed config and mounts
 * the navigation graph. All previous XML UI code is replaced by Compose.
 */
class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val repository = ConfigRepository(applicationContext)
        val machineRepository = dev.ohmyharness.mobile.data.MachineRepository(applicationContext)
        val llmRepository = dev.ohmyharness.mobile.data.LlmRepository(applicationContext)

        // Config gate must resolve synchronously before first frame; DataStore
        // reads are fast (memory cache after first load) so runBlocking is ok.
        val startSetup = kotlinx.coroutines.runBlocking {
            !repository.current().isComplete()
        }

        // Android 13+: notification permission is a runtime grant.
        if (android.os.Build.VERSION.SDK_INT >= 33 &&
            androidx.core.content.ContextCompat.checkSelfPermission(
                this, android.Manifest.permission.POST_NOTIFICATIONS,
            ) != android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            androidx.core.app.ActivityCompat.requestPermissions(
                this, arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001,
            )
        }

        setContent {
            // Type scale lives in config; the theme recomposes when Settings saves.
            val config by repository.config
                .collectAsStateWithLifecycle(initialValue = kotlinx.coroutines.runBlocking { repository.current() })
            OhmMobileTheme(fontScale = config.fontScale) {
                val viewModel: AppViewModel = androidx.lifecycle.viewmodel.compose.viewModel(
                    factory = AppViewModel.factory(repository, machineRepository, llmRepository, filesDir, applicationContext),
                )
                AppNavHost(viewModel = viewModel, startOnSetup = startSetup)
            }
        }
    }
}
