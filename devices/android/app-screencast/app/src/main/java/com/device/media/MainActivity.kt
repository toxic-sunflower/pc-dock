package com.device.media

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat

/**
 * Одноразовая точка входа: запрашивает захват экрана и запускает постоянный сервис.
 * Если сервис уже работает, сразу закрывается. Всем USB занимается сам сервис.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var projectionManager: MediaProjectionManager
    private val TAG = "DeviceMedia"

    /** Ушли в системные настройки разрешений — по возвращении показать их состояние снова или продолжить запуск */
    private var backFromSettings = false
    /** Окно разрешений уже показано при этом запуске с иконки */
    private var permsShown = false

    private val requestNotificationPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { isGranted ->
        Log.d(TAG, "POST_NOTIFICATIONS granted=$isGranted")
        requestProjection()
    }

    private val requestMediaProjection = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == RESULT_OK && result.data != null) {
            Log.d(TAG, "MediaProjection granted, запускаю сервис")
            val intent = Intent(this, ScreenCaptureService::class.java).apply {
                putExtra("resultCode", result.resultCode)
                putExtra("media_data", result.data)
            }
            startForegroundService(intent)
        } else {
            Log.e(TAG, "MediaProjection denied or cancelled: resultCode=${result.resultCode}")
            Toast.makeText(this, "Захват экрана отменён", Toast.LENGTH_SHORT).show()
        }
        finish()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.d(
            TAG, "MainActivity onCreate: intent.action=${intent?.action} " +
                    "device=${Build.MANUFACTURER} ${Build.MODEL} sdk=${Build.VERSION.SDK_INT}"
        )
        projectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        handleLaunch()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        Log.d(TAG, "MainActivity onNewIntent: action=${intent.action}")
        handleLaunch()
    }

    private fun handleLaunch() {
        // Открыли с иконки — показать разрешения для компьютера, если чего-то не хватает (при подключении по USB не спрашиваем)
        if (intent?.action == Intent.ACTION_MAIN && !permsShown && !allPermsGranted()) {
            permsShown = true
            showPermissions()
            return
        }
        continueLaunch()
    }

    private fun usageGranted() = ForegroundWatch.hasUsageAccess(this)
    private fun overlayGranted() = AppLauncher.canStartFromBackground(this)
    private fun uiGranted() = UiTreeService.enabledInSettings(this)
    private fun allPermsGranted() = usageGranted() && overlayGranted() && uiGranted()

    /**
     * Разрешения для компьютера (например, pynelo) — с состоянием и кнопкой в системные настройки (по одному: после
     * возврата окно показывается снова со следующим недостающим):
     *  • «Доступ к истории использования» — видеть, какое приложение на экране (сценарии только в разрешённых приложениях);
     *  • «Поверх других окон» — открывать приложение по команде компьютера (шаг «Открыть приложение»);
     *  • служба «Device Media: элементы экрана» (Специальные возможности) — видеть кнопки и надписи (нажатие по тексту).
     */
    private fun showPermissions() {
        val mark = { ok: Boolean -> if (ok) "✓ разрешено" else "✗ не разрешено" }
        val usage = usageGranted()
        val overlay = overlayGranted()
        val ui = uiGranted()
        val b = AlertDialog.Builder(this)
            .setTitle("Разрешения для компьютера")
            .setMessage(
                "Компьютер (например, pynelo) запускает сценарии только в разрешённых приложениях.\n\n" +
                    "• Доступ к истории использования — ${mark(usage)}\n" +
                    "  чтобы видеть, какое приложение открыто на экране (узнаётся только его имя, ничего не сохраняется).\n\n" +
                    "• Поверх других окон — ${mark(overlay)}\n" +
                    "  чтобы открывать приложение по команде компьютера (шаг «Открыть приложение»).\n\n" +
                    "• Элементы экрана (Специальные возможности) — ${mark(ui)}\n" +
                    "  чтобы сценарий находил кнопки и надписи по тексту. Текст паролей не передаётся, с экранов " +
                    "заблокированных хостом приложений ничего не передаётся, служба ничего не нажимает.\n\n" +
                    "В открывшемся списке выберите «Device Media» → «Разрешить» (для элементов экрана — включите службу)."
            )
            .setCancelable(false)
            .setNegativeButton(if (usage && overlay && ui) "Готово" else "Позже") { _, _ -> continueLaunch() }
        when {
            !usage -> b.setPositiveButton("История использования") { _, _ -> openSettings(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS)) }
            !overlay -> {
                val i = Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName"))
                b.setPositiveButton("Поверх других окон") { _, _ -> openSettings(i) }
            }
            !ui -> b.setPositiveButton("Специальные возможности") { _, _ -> openSettings(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)) }
        }
        // Если не хватает больше одного — второй кнопкой следующий (третий — после возврата)
        if (!usage && !overlay) {
            val i = Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName"))
            b.setNeutralButton("Поверх других окон") { _, _ -> openSettings(i) }
        } else if ((!usage || !overlay) && !ui) {
            b.setNeutralButton("Специальные возможности") { _, _ -> openSettings(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)) }
        }
        b.show()
    }

    private fun openSettings(i: Intent) {
        backFromSettings = true
        try {
            startActivity(i)
        } catch (e: Exception) {
            Log.w(TAG, "Нет экрана настроек ${i.action}: $e")
            backFromSettings = false
            continueLaunch()
        }
    }

    override fun onRestart() {
        super.onRestart()
        if (backFromSettings) {
            backFromSettings = false
            // Вернулись из настроек: показать, что изменилось (если всё разрешено — сразу дальше)
            if (allPermsGranted()) continueLaunch() else showPermissions()
        }
    }

    private fun continueLaunch() {
        // Сервис жив и проекция жива — делать нечего. Если проекция умерла
        // (система её остановила), запрашиваем разрешение заново в живой сервис.
        if (ScreenCaptureService.isRunning && !ScreenCaptureService.needsProjection()) {
            Log.d(TAG, "Сервис уже работает, закрываюсь")
            finish()
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, android.Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) {
            requestNotificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
            return
        }
        requestProjection()
    }

    private fun requestProjection() {
        requestMediaProjection.launch(projectionManager.createScreenCaptureIntent())
    }

    override fun onDestroy() {
        Log.d(TAG, "MainActivity onDestroy")
        super.onDestroy()
    }
}
