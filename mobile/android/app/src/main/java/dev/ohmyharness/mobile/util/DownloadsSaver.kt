package dev.ohmyharness.mobile.util

import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import java.io.File

/**
 * Saves exported files (markdown/long-screenshot PNG) into the device's
 * public Downloads folder so they are directly visible in any file manager
 * — no share sheet needed.
 *
 * API 29+ uses MediaStore.Downloads (no permission required). API 28 falls
 * back to the app-specific external dir (no permission, still reachable
 * from most file managers under Android/data).
 */
object DownloadsSaver {

    /** @return a user-facing location string, or null on failure. */
    fun save(context: Context, source: File, mimeType: String): String? = runCatching {
        val displayName = source.name
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, displayName)
                put(MediaStore.Downloads.MIME_TYPE, mimeType)
            }
            val uri = context.contentResolver.insert(
                MediaStore.Downloads.EXTERNAL_CONTENT_URI, values,
            ) ?: return null
            context.contentResolver.openOutputStream(uri)?.use { out ->
                source.inputStream().use { it.copyTo(out) }
            } ?: return null
            "Downloads/$displayName"
        } else {
            // API 28: app-specific external storage; no permission needed.
            val dir = context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS)
                ?: return null
            dir.mkdirs()
            val dest = File(dir, displayName)
            source.copyTo(dest, overwrite = true)
            "Android/data/${context.packageName}/files/Download/$displayName"
        }
    }.getOrNull()
}
