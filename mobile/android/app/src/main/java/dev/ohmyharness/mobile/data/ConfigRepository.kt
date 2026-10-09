package dev.ohmyharness.mobile.data

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.floatPreferencesKey
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
    val apiKey: String = "",
    val model: String = "",
    val llmBaseUrl: String = "",
    val provider: String = "openai",
    val allowInsecureHttp: Boolean = false,
    /** Global type scale multiplier applied by the theme (0.85..1.3). */
    val fontScale: Float = 1.0f,
    /** Tool output display mode in chat ("tail" | "summary" | "hidden"). */
    val toolDisplay: String = "tail",
)

private val Context.configStore by preferencesDataStore(name = "config")

/** True when all fields required for LLM access are present. */
fun AppConfig.isComplete(): Boolean =
    apiKey.isNotBlank() && model.isNotBlank()


/** Reads and writes [AppConfig] via Preferences DataStore. */
class ConfigRepository(private val context: android.content.Context) {

    private object Keys {
        val apiKey = stringPreferencesKey("api_key")
        val model = stringPreferencesKey("model")
        val llmBaseUrl = stringPreferencesKey("llm_base_url")
        val provider = stringPreferencesKey("provider")
        val allowInsecureHttp = booleanPreferencesKey("allow_insecure_http")
        val fontScale = floatPreferencesKey("font_scale")
        val toolDisplay = stringPreferencesKey("tool_display")
        // Legacy gateway fields (pre machine split); read once for migration.
        val legacyGatewayUrl = stringPreferencesKey("gateway_url")
        val legacyGatewayToken = stringPreferencesKey("gateway_token")
        val legacyTrustProfile = stringPreferencesKey("trust_profile")
        val migrated = booleanPreferencesKey("gateway_fields_migrated")
    }

    val config: Flow<AppConfig> = context.configStore.data.map { prefs ->
        AppConfig(
            apiKey = prefs[Keys.apiKey] ?: "",
            model = prefs[Keys.model] ?: "",
            llmBaseUrl = prefs[Keys.llmBaseUrl] ?: "",
            provider = prefs[Keys.provider] ?: "openai",
            allowInsecureHttp = prefs[Keys.allowInsecureHttp] ?: false,
            fontScale = prefs[Keys.fontScale] ?: 1.0f,
            toolDisplay = prefs[Keys.toolDisplay] ?: "tail",
        )
    }

    suspend fun current(): AppConfig {
        val saved = config.first()
        if (saved.isComplete()) return saved
        // First boot: seed LLM fields from bundled defaults.
        val defaults = seedDefaults() ?: return saved
        if (saved.apiKey.isBlank() && defaults.apiKey.isNotBlank()) {
            save(defaults)
            return defaults
        }
        return saved
    }

    /** One-time migration from the pre-machine-split flat config. */
    suspend fun migrateLegacy(machines: MachineRepository, llm: LlmRepository) {
        val prefs = context.configStore.data.first()
        if (prefs[Keys.migrated] == true) return
        val legacyUrl = prefs[Keys.legacyGatewayUrl]
        if (!legacyUrl.isNullOrBlank() && machines.current().none { it.gatewayUrl == legacyUrl }) {
            machines.upsert(
                Machine(
                    id = MachineRepository.newId(),
                    name = "Machine 1",
                    gatewayUrl = legacyUrl,
                    gatewayToken = prefs[Keys.legacyGatewayToken] ?: "",
                    trustProfile = prefs[Keys.legacyTrustProfile] ?: "restricted",
                    createdAtMs = System.currentTimeMillis(),
                ),
            )
        }
        if (llm.current().isEmpty() && !prefs[Keys.apiKey].isNullOrBlank()) {
            llm.upsert(
                LlmProfile(
                    id = MachineRepository.newId(),
                    name = "Default",
                    provider = prefs[Keys.provider] ?: "openai",
                    apiKey = prefs[Keys.apiKey]!!,
                    model = prefs[Keys.model] ?: "",
                    baseUrl = prefs[Keys.llmBaseUrl] ?: "",
                ),
            )
        }
        // Fresh installs: seed the first machine from defaults.json.
        if (machines.current().isEmpty()) {
            seedDefaultsMachines()?.forEach { machines.upsert(it) }
        }
        context.configStore.edit { it[Keys.migrated] = true }
    }

    /** Machines declared in the bundled defaults.json (fresh installs). */
    private suspend fun seedDefaultsMachines(): List<Machine>? = runCatching {
        val json = context.assets.open("defaults.json").bufferedReader().use { it.readText() }
        val arr = org.json.JSONObject(json).optJSONArray("machines") ?: return null
        (0 until arr.length()).map { i ->
            val obj = arr.getJSONObject(i)
            Machine(
                id = MachineRepository.newId(),
                name = obj.optString("name", "Machine"),
                gatewayUrl = obj.optString("gatewayUrl"),
                gatewayToken = obj.optString("gatewayToken"),
                trustProfile = obj.optString("trustProfile", "restricted"),
                createdAtMs = System.currentTimeMillis(),
            )
        }
    }.getOrNull()

    /** Merge bundled defaults.json over empty state (LLM fields only). */
    private suspend fun seedDefaults(): AppConfig? = runCatching {
        val json = context.assets.open("defaults.json").bufferedReader().use { it.readText() }
        val obj = org.json.JSONObject(json)
        AppConfig(
            apiKey = obj.optString("apiKey"),
            model = obj.optString("model"),
            llmBaseUrl = obj.optString("llmBaseUrl"),
            provider = obj.optString("provider", "openai"),
            allowInsecureHttp = obj.optBoolean("allowInsecureHttp", false),
            fontScale = 1.0f,
        )
    }.getOrNull()

    suspend fun save(config: AppConfig) {
        context.configStore.edit { prefs ->
            prefs[Keys.apiKey] = config.apiKey
            prefs[Keys.model] = config.model
            prefs[Keys.llmBaseUrl] = config.llmBaseUrl
            prefs[Keys.provider] = config.provider
            prefs[Keys.allowInsecureHttp] = config.allowInsecureHttp
            prefs[Keys.fontScale] = config.fontScale
            prefs[Keys.toolDisplay] = config.toolDisplay
        }
    }
}
