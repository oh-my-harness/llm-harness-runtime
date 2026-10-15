package dev.ohmyharness.mobile.util

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Handler
import android.os.Looper
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import android.webkit.WebViewClient
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/**
 * Renders an HTML transcript into a full-length PNG ("long screenshot").
 *
 * A WebView is attached off-screen to the anchor's window, measured at full
 * content height, drawn into a bitmap, then detached. Synchronous from the
 * caller's perspective (worker thread blocks up to 15s).
 */
object LongScreenshot {

    private const val TIMEOUT_SECONDS = 15L
    private const val CONTENT_WIDTH_PX = 1080

    /** @return the PNG file, or null when capture failed or timed out. */
    fun capture(context: Context, anchor: View, title: String, html: String): File? {
        if (Looper.myLooper() == Looper.getMainLooper()) return null
        val latch = CountDownLatch(1)
        val resultRef = AtomicReference<Bitmap?>(null)

        Handler(Looper.getMainLooper()).post {
            try {
                captureOnUiThread(context, anchor, title, html) { bmp ->
                    resultRef.set(bmp)
                    latch.countDown()
                }
            } catch (e: Exception) {
                resultRef.set(null)
                latch.countDown()
            }
        }

        latch.await(TIMEOUT_SECONDS, TimeUnit.SECONDS)
        val bmp = resultRef.get() ?: return null
        return runCatching { savePng(context, bmp, title) }.getOrNull()
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun captureOnUiThread(
        context: Context,
        anchor: View,
        title: String,
        html: String,
        onResult: (Bitmap?) -> Unit,
    ) {
        val density = context.resources.displayMetrics.density
        val widthPx = (CONTENT_WIDTH_PX * density).toInt()

        val webView = WebView(context)
        webView.layoutParams = ViewGroup.LayoutParams(widthPx, ViewGroup.LayoutParams.WRAP_CONTENT)
        webView.setBackgroundColor(android.graphics.Color.WHITE)
        webView.visibility = View.INVISIBLE
        webView.alpha = 0f
        webView.isFocusable = false

        var completed = false
        fun finish(bmp: Bitmap?) {
            if (completed) return
            completed = true
            (webView.parent as? ViewGroup)?.removeView(webView)
            webView.destroy()
            onResult(bmp)
        }

        webView.webViewClient = object : WebViewClient() {
            override fun onPageFinished(view: WebView, url: String) {
                // Give the renderer a beat to lay out images/fonts, then
                // measure the full content height and draw it all.
                view.postDelayed({
                    runCatching {
                        view.measure(
                            View.MeasureSpec.makeMeasureSpec(widthPx, View.MeasureSpec.EXACTLY),
                            View.MeasureSpec.makeMeasureSpec(0, View.MeasureSpec.UNSPECIFIED),
                        )
                        view.layout(0, 0, widthPx, view.measuredHeight.coerceAtLeast(1))
                        val bmp = Bitmap.createBitmap(
                            widthPx,
                            view.measuredHeight.coerceIn(1, 30_000),
                            Bitmap.Config.ARGB_8888,
                        )
                        view.draw(Canvas(bmp))
                        finish(bmp)
                    }.onFailure { finish(null) }
                }, 300)
            }
        }
        webView.loadDataWithBaseURL(null, html, "text/html", "utf-8", null)

        // Off-screen attachment: the anchor's root view hosts the webview so
        // it can lay out without being visible.
        val root = anchor.rootView as? ViewGroup
        if (root == null) {
            finish(null)
            return
        }
        webView.x = -widthPx.toFloat() * 2f // far off-screen
        root.addView(
            webView,
            ViewGroup.LayoutParams(widthPx, ViewGroup.LayoutParams.WRAP_CONTENT),
        )
    }

    private fun savePng(context: Context, bmp: Bitmap, title: String): File {
        val dir = File(context.cacheDir, "exports").apply { mkdirs() }
        val safe = title.replace(Regex("[^A-Za-z0-9\\u4e00-\\u9fa5-_ ]"), "_").take(60).ifBlank { "session" }
        val file = File(dir, "$safe.png")
        file.outputStream().use { out ->
            bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
        }
        bmp.recycle()
        return file
    }
}
