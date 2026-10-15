package dev.ohmyharness.mobile.task

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import dev.ohmyharness.mobile.MainActivity

/**
 * Foreground keepalive while any task is running: an ongoing low-importance
 * notification keeps the process (and its event pumps) alive when the screen
 * turns off. Also posts task-completion notifications.
 */
class TaskKeepaliveService : android.app.Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val running = intent?.getIntExtra(EXTRA_RUNNING, 0) ?: 0
        if (running <= 0) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        val notification = NotificationCompat.Builder(this, CHANNEL_KEEPALIVE)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("oh-my-harness")
            .setContentText("$running task(s) running")
            .setOngoing(true)
            .setContentIntent(tapIntent())
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                KEEPALIVE_ID, notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(KEEPALIVE_ID, notification)
        }
        return START_NOT_STICKY
    }

    private fun tapIntent(): PendingIntent =
        PendingIntent.getActivity(
            this, 0,
            Intent(this, dev.ohmyharness.mobile.MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

    companion object {
        private const val CHANNEL_KEEPALIVE = "task-keepalive"
        private const val CHANNEL_RESULTS = "task-results"
        private const val KEEPALIVE_ID = 1

        const val EXTRA_RUNNING = "running"

        fun ensureChannels(context: Context) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
            val manager = context.getSystemService(NotificationManager::class.java) ?: return
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_KEEPALIVE, "Running tasks", NotificationManager.IMPORTANCE_LOW),
            )
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_RESULTS, "Task results", NotificationManager.IMPORTANCE_DEFAULT),
            )
        }

        /** Completion notification; tap opens the app. */
        fun notifyTaskFinished(context: Context, title: String, body: String) {
            ensureChannels(context)
            val intent = Intent(context, dev.ohmyharness.mobile.MainActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
            }
            val notification = NotificationCompat.Builder(context, CHANNEL_RESULTS)
                .setSmallIcon(android.R.drawable.stat_sys_download_done)
                .setContentTitle(title)
                .setContentText(body)
                .setAutoCancel(true)
                .setContentIntent(
                    PendingIntent.getActivity(
                        context, title.hashCode(), intent,
                        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                    ),
                )
                .build()
            runCatching {
                NotificationManagerCompat.from(context).notify(title.hashCode(), notification)
            }
        }

        /** Start/refresh the keepalive foreground notification. */
        fun start(context: Context, running: Int) {
            ensureChannels(context)
            val intent = Intent(context, TaskKeepaliveService::class.java)
                .putExtra(EXTRA_RUNNING, running)
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, TaskKeepaliveService::class.java))
        }
    }
}
