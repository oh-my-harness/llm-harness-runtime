package dev.ohmyharness.mobile.data

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.util.UUID

/** One computer/server the app can delegate work to. */
data class Machine(
    val id: String,
    val name: String,
    /** Gateway base URL; blank = sandbox-free machine (pure chat, no tools). */
    val gatewayUrl: String,
    val gatewayToken: String,
    val trustProfile: String,
    val createdAtMs: Long,
) {
    val isSandboxFree: Boolean get() = gatewayUrl.isBlank()
}

private val Context.machineStore by preferencesDataStore(name = "machines")

/** Decode/encode helpers over org.json (same style as ConfigRepository). */
internal fun machinesFromJson(raw: String): List<Machine> = runCatching {
    val arr = org.json.JSONArray(raw)
    (0 until arr.length()).map { i ->
        val obj = arr.getJSONObject(i)
        Machine(
            id = obj.getString("id"),
            name = obj.optString("name", "machine"),
            gatewayUrl = obj.optString("gatewayUrl"),
            gatewayToken = obj.optString("gatewayToken"),
            trustProfile = obj.optString("trustProfile", "restricted"),
            createdAtMs = obj.optLong("createdAtMs"),
        )
    }
}.getOrDefault(emptyList())

internal fun machinesToJson(machines: List<Machine>): String {
    val arr = org.json.JSONArray()
    machines.forEach {
        arr.put(
            org.json.JSONObject()
                .put("id", it.id)
                .put("name", it.name)
                .put("gatewayUrl", it.gatewayUrl)
                .put("gatewayToken", it.gatewayToken)
                .put("trustProfile", it.trustProfile)
                .put("createdAtMs", it.createdAtMs),
        )
    }
    return arr.toString()
}

/** CRUD for machines, persisted as a JSON list in DataStore. */
class MachineRepository(private val context: Context) {

    private object Keys {
        val machines = stringPreferencesKey("machines_json")
    }

    val machines: Flow<List<Machine>> = context.machineStore.data.map { prefs ->
        machinesFromJson(prefs[Keys.machines] ?: "[]")
    }

    suspend fun current(): List<Machine> = machines.first()

    suspend fun upsert(machine: Machine) {
        context.machineStore.edit { prefs ->
            val list = machinesFromJson(prefs[Keys.machines] ?: "[]").toMutableList()
            list.removeAll { it.id == machine.id }
            list.add(machine)
            prefs[Keys.machines] = machinesToJson(list)
        }
    }

    suspend fun delete(id: String) {
        context.machineStore.edit { prefs ->
            val list = machinesFromJson(prefs[Keys.machines] ?: "[]").filter { it.id != id }
            prefs[Keys.machines] = machinesToJson(list)
        }
    }

    companion object {
        fun newId(): String = UUID.randomUUID().toString()

        /** Probe `{gatewayUrl}/healthz`. Blank URL (chat-only machine) is always "up". */
        suspend fun probe(gatewayUrl: String): Boolean = withContext(Dispatchers.IO) {
            if (gatewayUrl.isBlank()) return@withContext true
            runCatching {
                val conn = URL("${gatewayUrl.trimEnd('/')}/healthz").openConnection()
                    as HttpURLConnection
                conn.connectTimeout = 2_000
                conn.readTimeout = 2_000
                try {
                    conn.responseCode in 200..299
                } finally {
                    conn.disconnect()
                }
            }.getOrDefault(false)
        }
    }
}

/** One LLM API configuration; shared across machines (LLM is app-level). */
data class LlmProfile(
    val id: String,
    val name: String,
    val provider: String,
    val apiKey: String,
    val model: String,
    val baseUrl: String,
)


private val Context.llmStore by preferencesDataStore(name = "llm")

internal fun llmProfilesFromJson(raw: String): List<LlmProfile> = runCatching {
    val arr = JSONArray(raw)
    (0 until arr.length()).map { i ->
        val obj = arr.getJSONObject(i)
        LlmProfile(
            id = obj.getString("id"),
            name = obj.optString("name", "profile"),
            provider = obj.optString("provider", "openai"),
            apiKey = obj.optString("apiKey"),
            model = obj.optString("model"),
            baseUrl = obj.optString("baseUrl"),
        )
    }
}.getOrDefault(emptyList())

internal fun llmProfilesToJson(profiles: List<LlmProfile>): String {
    val arr = org.json.JSONArray()
    profiles.forEach {
        arr.put(
            JSONObject()
                .put("id", it.id)
                .put("name", it.name)
                .put("provider", it.provider)
                .put("apiKey", it.apiKey)
                .put("model", it.model)
                .put("baseUrl", it.baseUrl),
        )
    }
    return arr.toString()
}

/** App-level LLM profiles; exactly one active at a time. */
class LlmRepository(private val context: Context) {

    private object Keys {
        val profiles = stringPreferencesKey("llm_profiles_json")
        val activeId = stringPreferencesKey("llm_active_id")
    }

    val profiles: Flow<List<LlmProfile>> = context.llmStore.data.map { prefs ->
        llmProfilesFromJson(prefs[Keys.profiles] ?: "[]")
    }

    suspend fun current(): List<LlmProfile> = profiles.first()

    suspend fun active(): LlmProfile? {
        val list = current()
        val activeId = context.llmStore.data.first()[Keys.activeId]
        return list.firstOrNull { it.id == activeId } ?: list.firstOrNull()
    }

    suspend fun setActive(id: String) {
        context.llmStore.edit { it[Keys.activeId] = id }
    }

    suspend fun upsert(profile: LlmProfile) {
        context.llmStore.edit { prefs ->
            val list = llmProfilesFromJson(prefs[Keys.profiles] ?: "[]").toMutableList()
            list.removeAll { it.id == profile.id }
            list.add(profile)
            prefs[Keys.profiles] = llmProfilesToJson(list)
            if (prefs[Keys.activeId] == null) prefs[Keys.activeId] = profile.id
        }
    }

    suspend fun delete(id: String) {
        context.llmStore.edit { prefs ->
            val list = llmProfilesFromJson(prefs[Keys.profiles] ?: "[]").filter { it.id != id }
            prefs[Keys.profiles] = llmProfilesToJson(list)
            if (prefs[Keys.activeId] == id) {
                val next = list.firstOrNull()?.id
                if (next != null) prefs[Keys.activeId] = next
            }
        }
    }
}
