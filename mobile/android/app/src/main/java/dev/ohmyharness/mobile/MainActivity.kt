package dev.ohmyharness.mobile

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
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

        // Config gate must resolve synchronously before first frame; DataStore
        // reads are fast (memory cache after first load) so runBlocking is ok.
        val startSetup = kotlinx.coroutines.runBlocking {
            !repository.current().isComplete()
        }

        setContent {
            OhmMobileTheme {
                val viewModel: AppViewModel = androidx.lifecycle.viewmodel.compose.viewModel(
                    factory = AppViewModel.factory(repository, filesDir),
                )
                AppNavHost(viewModel = viewModel, startOnSetup = startSetup)
            }
        }
    }
}
