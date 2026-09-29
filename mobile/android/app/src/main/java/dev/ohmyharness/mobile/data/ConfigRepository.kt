package dev.ohmyharness.mobile.data

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

/**
 * Connection configuration, persisted in DataStore. Field names mirror the
 * Rust `MobileConfig` record so the mapping stays mechanical.
 */
data class AppConfig(
    val gatewayUrl: String = "",
    val gatewayToken: String = "",
    val apiKey: String = "",
    val model: String = "",
    val llmBaseUrl: String = "",
    val provider: String = "openai",
    val allowInsecureHttp: Boolean = false,
    val trustProfile: String = "restricted",
)

private val Context.configStore by preferencesDataStore(name = "config")

/** True when all fields required to connect are present. */
fun AppConfig.isComplete(): Boolean =
    gatewayUrl.isNotBlank() && gatewayToken.isNotBlank() &&
        apiKey.isNotBlank() && model.isNotBlank()


/** Reads and writes [AppConfig] via Preferences DataStore. */
class ConfigRepository(private val context: android.content.Context) {

    private object Keys {
        val gatewayUrl = stringPreferencesKey("gateway_url")
        val gatewayToken = stringPreferencesKey("gateway_token")
        val apiKey = stringPreferencesKey("api_key")
        val model = stringPreferencesKey("model")
        val llmBaseUrl = stringPreferencesKey("llm_base_url")
        val provider = stringPreferencesKey("provider")
        val allowInsecureHttp = booleanPreferencesKey("allow_insecure_http")
        val trustProfile = stringPreferencesKey("trust_profile")
    }

    val config: Flow<AppConfig> = context.configStore.data.map { prefs ->
        AppConfig(
            gatewayUrl = prefs[Keys.gatewayUrl] ?: "",
            gatewayToken = prefs[Keys.gatewayToken] ?: "",
            apiKey = prefs[Keys.apiKey] ?: "",
            model = prefs[Keys.model] ?: "",
            llmBaseUrl = prefs[Keys.llmBaseUrl] ?: "",
            provider = prefs[Keys.provider] ?: "openai",
            allowInsecureHttp = prefs[Keys.allowInsecureHttp] ?: false,
            trustProfile = prefs[Keys.trustProfile] ?: "restricted",
        )
    }

    suspend fun current(): AppConfig {
        val saved = config.first()
        if (saved.isComplete()) return saved
        // First boot: seed from bundled defaults (test-phase full plaintext).
        // First boot: seed from bundled defaults (api key still manual).
        val defaults = seedDefaults() ?: return saved
        if (saved.gatewayUrl.isBlank() && defaults.gatewayUrl.isNotBlank()) {
            save(defaults)
            return defaults
        }
        return saved
    }

    /** Merge bundled defaults.json over the current values (key never seeded). */
    private suspend fun seedDefaults(): AppConfig? = runCatching {
        val json = context.assets.open("defaults.json").bufferedReader().use { it.readText() }
        val obj = org.json.JSONObject(json)
        AppConfig(
            gatewayUrl = obj.optString("gatewayUrl"),
            gatewayToken = obj.optString("gatewayToken"),
            apiKey = obj.optString("apiKey"),
            model = obj.optString("model"),
            llmBaseUrl = obj.optString("llmBaseUrl"),
            provider = obj.optString("provider", "openai"),
            allowInsecureHttp = obj.optBoolean("allowInsecureHttp", false),
            trustProfile = obj.optString("trustProfile", "restricted"),
        )
    }.getOrNull()

    suspend fun save(config: AppConfig) {
        context.configStore.edit { prefs ->
            prefs[Keys.gatewayUrl] = config.gatewayUrl
            prefs[Keys.gatewayToken] = config.gatewayToken
            prefs[Keys.apiKey] = config.apiKey
            prefs[Keys.model] = config.model
            prefs[Keys.llmBaseUrl] = config.llmBaseUrl
            prefs[Keys.provider] = config.provider
            prefs[Keys.allowInsecureHttp] = config.allowInsecureHttp
            prefs[Keys.trustProfile] = config.trustProfile
        }
    }
}
