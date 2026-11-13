package com.device.media

import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.hardware.usb.UsbManager
import android.media.AudioManager
import android.media.Image
import android.os.PowerManager
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.io.ByteArrayOutputStream
import java.io.IOException

/**
 * Живёт постоянно: держит MediaProjection (диалог захвата нужен один раз), сам подхватывает USB-аксессуар
 * и говорит с ПК по протоколу v2 (PROTOCOL.md). Без HELLO от ПК сессия не поднимается: один протокол,
 * запасного режима нет.
 *
 * Всё состояние меняется только в потоке [handler]; потоки чтения и записи USB живут в [Link].
 */
class ScreenCaptureService : Service(), HostLink.Listener {
    companion object {
        @Volatile
        var isRunning = false

        @Volatile
        private var projectionAlive = false

        /** Проекция мертва (токен одноразовый): нужен новый диалог записи экрана. */
        fun needsProjection() = isRunning && !projectionAlive
        private const val ACTION_USB_PERMISSION = "com.device.media.USB_PERMISSION"
        private const val ACTION_STOP = "com.device.media.STOP"
        private const val CHANNEL_ID = "dock_channel"
        private const val DEFAULT_JPEG_QUALITY = 70
        private const val USB_POLL_MS = 1000L
        private const val HOST_SILENT_MS = 6000L
        private const val IDLE_FRAME_INTERVAL_MS = 1000L
        private const val NAME = "pcdock-android/1.0"
        /** Опрос переднего плана для хоста (STATUS 16): смена — сразу, иначе повтор раз в секунду */
        private const val FOREGROUND_POLL_MS = 500L
        private const val FOREGROUND_REPEAT_MS = 1000L
    }

    private class Layout(
        val streamW: Int,
        val streamH: Int,
        val screenW: Int,
        val screenH: Int,
        val dpi: Int,
        val rotation: Int,
    ) {
        fun sameAs(o: Layout) = streamW == o.streamW && streamH == o.streamH && screenW == o.screenW &&
                screenH == o.screenH && dpi == o.dpi && rotation == o.rotation
    }

    private class StreamConfig(
        var video: Boolean = true,
        var audio: Boolean = false,
        var maxWidth: Int = 0,
        var maxHeight: Int = 0,
        var jpegQuality: Int = DEFAULT_JPEG_QUALITY,
        var maxFps: Int = 0,
    )

    private val TAG = "DeviceMedia"

    private var mediaProjection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var imageReader: ImageReader? = null
    private lateinit var usbManager: UsbManager
    private lateinit var displayManager: DisplayManager
    private var captureThread: HandlerThread? = null
    private var handler: Handler? = null
    private var receiverRegistered = false
    private var displayListenerRegistered = false
    private var permissionRequested = false

    // Состояние сессии
    private var link: HostLink? = null
    private var sessionActive = false
    private var lastHostRx = 0L
    private var stream = StreamConfig()
    private var layout: Layout? = null

    // Кадры
    private var frameBitmap: Bitmap? = null
    private val jpegBuffer = ByteArrayOutputStream()
    private var lastJpeg: ByteArray? = null
    private var lastPtsUs = 0L
    private var lastConvertMs = 0L
    private var pendingImage: Image? = null
    private var trailingScheduled = false

    // Звук: захват через ту же проекцию + глушим локальный вывод, как будто мы колонка
    private var audioCapture: AudioCapture? = null
    private var audioManager: AudioManager? = null
    private var prevMusicVolume = -1
    private var audioActive = false
    private var audioFramesSent = 0L

    // Экран не гаснет, пока идёт док-сессия: иначе при блокировке нечего захватывать
    // и PIN-код не ввести. Гасится обратно при отключении USB.
    private var dockWakeLock: PowerManager.WakeLock? = null

    // Статистика для логов
    private var startedAt = 0L
    private var running = false
    private var imageCallbacks = 0
    private var framesSent = 0
    private var framesThrottled = 0
    private var connects = 0

    // Передний план для хоста (PROTOCOL.md: CAP_FOREGROUND, STATUS 16/17) — только если хост попросил в HELLO
    private var foregroundWatch: ForegroundWatch? = null
    private var hostWantsForeground = false
    private var foregroundSent: String? = null
    private var foregroundSentAt = 0L
    // Запуск приложений по команде хоста (CAP_LAUNCH_APP, LAUNCH_APP → STATUS 18) — только если хост поставил бит в HELLO
    private var hostWantsLaunch = false
    // Список приложений по команде хоста (CAP_LIST_APPS, LIST_APPS → STATUS 19)
    private var hostWantsApps = false
    // Элементы экрана (CAP_UI_TREE, UI_DUMP → STATUS 20, «экран изменился» → STATUS 21) и падения (CAP_EXIT_INFO → STATUS 22)
    @Volatile private var hostWantsUi = false
    @Volatile private var hostWantsExit = false
    // IP-адрес телефона (CAP_IP_CHECK, IP_CHECK → STATUS 23)
    @Volatile private var hostWantsIp = false
    private val ipExecutor = java.util.concurrent.Executors.newSingleThreadExecutor { r -> Thread(r, "ip-check") }
    // HTTP в локальной Wi-Fi-сети (CAP_HTTP_LOCAL, HTTP_LOCAL → STATUS 24): свой поток — медленный роутер не задерживает IP_CHECK
    @Volatile private var hostWantsHttp = false
    private val httpExecutor = java.util.concurrent.Executors.newSingleThreadExecutor { r -> Thread(r, "http-local") }
    // Ввод текста (CAP_TEXT_INPUT, TEXT_INPUT → STATUS 25): по одному, по порядку, не в потоке захвата
    @Volatile private var hostWantsText = false
    private val textExecutor = java.util.concurrent.Executors.newSingleThreadExecutor { r -> Thread(r, "text-input") }
    // Дампы и причины завершения — по одному, не в потоке захвата (обход дерева — межпроцессные вызовы)
    private val uiExecutor = java.util.concurrent.Executors.newSingleThreadExecutor { r -> Thread(r, "ui-tree") }

    private val usbReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            Log.d(TAG, "USB broadcast: ${intent.action}")
            when (intent.action) {
                ACTION_USB_PERMISSION -> handler?.post { tryConnect() }
                UsbManager.ACTION_USB_ACCESSORY_DETACHED -> handler?.post {
                    link?.close("ACCESSORY_DETACHED")
                    permissionRequested = false
                }
            }
        }
    }

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) relayout("onDisplayChanged")
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.d(
            TAG, "Service onStartCommand: startId=$startId running=$running " +
                    "device=${Build.MANUFACTURER} ${Build.MODEL} sdk=${Build.VERSION.SDK_INT}"
        )
        if (intent?.action == ACTION_STOP) {
            Log.d(TAG, "Получена команда STOP из уведомления")
            stopSelf()
            return START_NOT_STICKY
        }
        createNotificationChannel()
        showRunningNotification()

        // Тип сервиса нужно указывать явно, иначе на Android 14+ getMediaProjection() может упасть
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                1, buildNotification("Экран телефона передаётся на ПК", false),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            )
        } else {
            startForeground(1, buildNotification("Экран телефона передаётся на ПК", false))
        }

        val resultCode = intent?.getIntExtra("resultCode", Activity.RESULT_CANCELED) ?: Activity.RESULT_CANCELED
        val data = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent?.getParcelableExtra("media_data", Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent?.getParcelableExtra("media_data") as? Intent
        }

        if (running) {
            // Сервис жив (например, проекция умерла и ждёт нового разрешения):
            // новый токен — перезапустить захват, без токена — игнорировать.
            if (resultCode == Activity.RESULT_OK && data != null && !projectionAlive) {
                restartProjection(resultCode, data)
            } else {
                Log.d(TAG, "Сервис уже работает, повторный старт игнорирую")
            }
            return START_NOT_STICKY
        }

        if (resultCode == Activity.RESULT_OK && data != null) {
            startedAt = SystemClock.elapsedRealtime()
            usbManager = getSystemService(Context.USB_SERVICE) as UsbManager
            displayManager = getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            val projectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            mediaProjection = projectionManager.getMediaProjection(resultCode, data)
            Log.d(TAG, "getMediaProjection -> $mediaProjection")
            registerUsbReceiver()
            startCapture()
        } else {
            Log.e(TAG, "Ошибка: resultCode=$resultCode, data=$data")
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun registerUsbReceiver() {
        val filter = IntentFilter().apply {
            addAction(ACTION_USB_PERMISSION)
            addAction(UsbManager.ACTION_USB_ACCESSORY_DETACHED)
        }
        ContextCompat.registerReceiver(this, usbReceiver, filter, ContextCompat.RECEIVER_NOT_EXPORTED)
        receiverRegistered = true
    }

    // ---------- экран и поворот ----------

    private fun computeLayout(): Layout {
        val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
        val m = DisplayMetrics()
        @Suppress("DEPRECATION")
        wm.defaultDisplay.getRealMetrics(m)
        @Suppress("DEPRECATION")
        val rotation = wm.defaultDisplay.rotation
        val cfg = stream
        var w: Int
        var h: Int
        if (cfg.maxWidth == 0 && cfg.maxHeight == 0) {
            // Разрешение по умолчанию всегда оригинальное (решение 34): масштаб задаёт только явный SET_STREAM.
            w = m.widthPixels
            h = m.heightPixels
        } else {
            var scale = 1.0
            if (cfg.maxWidth > 0) scale = minOf(scale, cfg.maxWidth.toDouble() / m.widthPixels)
            if (cfg.maxHeight > 0) scale = minOf(scale, cfg.maxHeight.toDouble() / m.heightPixels)
            w = (m.widthPixels * scale).toInt()
            h = (m.heightPixels * scale).toInt()
        }
        // Чётные размеры (иначе некоторые кодеки капризничают)
        w = (w and 1.inv()).coerceAtLeast(2)
        h = (h and 1.inv()).coerceAtLeast(2)
        return Layout(w, h, m.widthPixels, m.heightPixels, m.densityDpi, rotation)
    }

    private fun newReader(l: Layout): ImageReader {
        val r = ImageReader.newInstance(l.streamW, l.streamH, PixelFormat.RGBA_8888, 2)
        r.setOnImageAvailableListener({ onImage(it) }, handler)
        return r
    }

    private fun startCapture() {
        Log.d(TAG, "startCapture called")
        val projection = mediaProjection ?: return
        val thread = HandlerThread("capture").also { it.start() }
        captureThread = thread
        val h = Handler(thread.looper)
        handler = h

        // Android 14+: callback обязателен до createVirtualDisplay
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.w(TAG, "MediaProjection.onStop (трансляция остановлена системой/пользователем)")
                handler?.post { onProjectionStopped() }
            }
        }, h)

        val l = computeLayout()
        layout = l
        Log.d(
            TAG,
            "Экран: ${l.screenW}x${l.screenH} dpi=${l.dpi} rot=${l.rotation} -> захват ${l.streamW}x${l.streamH}"
        )
        val reader = newReader(l)
        imageReader = reader
        val vd = projection.createVirtualDisplay(
            "ScreenCapture",
            l.streamW, l.streamH, l.dpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            reader.surface,
            object : VirtualDisplay.Callback() {
                override fun onPaused() {
                    Log.d(TAG, "VirtualDisplay.onPaused")
                }

                override fun onResumed() {
                    Log.d(TAG, "VirtualDisplay.onResumed")
                }

                override fun onStopped() {
                    Log.w(TAG, "VirtualDisplay.onStopped")
                }
            },
            h
        )
        virtualDisplay = vd
        Log.d(TAG, "createVirtualDisplay -> $vd, state=${vd?.display?.state}")
        if (vd == null) {
            Log.e(TAG, "VirtualDisplay не создан (null), останавливаю сервис")
            stopSelf()
            return
        }
        isRunning = true
        running = true
        projectionAlive = true

        displayManager.registerDisplayListener(displayListener, h)
        displayListenerRegistered = true

        h.post(object : Runnable {
            override fun run() {
                try {
                    if (!running) return
                    tick()
                } catch (e: Throwable) {
                    Log.e(TAG, "Сбой tick, продолжаю опрос", e)
                } finally {
                    if (running) h.postDelayed(this, USB_POLL_MS)
                }
            }
        })
        h.postDelayed(object : Runnable {
            override fun run() {
                try {
                    if (!running) return
                    logHeartbeat()
                } catch (e: Throwable) {
                    Log.e(TAG, "Сбой heartbeat", e)
                } finally {
                    if (running) h.postDelayed(this, 5000)
                }
            }
        }, 5000)
        h.postDelayed(object : Runnable {
            override fun run() {
                try {
                    if (!running) return
                    reportForeground(false)
                } catch (e: Throwable) {
                    Log.e(TAG, "Сбой опроса переднего плана", e)
                } finally {
                    if (running) h.postDelayed(this, FOREGROUND_POLL_MS)
                }
            }
        }, FOREGROUND_POLL_MS)
    }

    /** Проекция умерла (система/пользователь): сервис и USB живут, ждём нового разрешения. */
    private fun onProjectionStopped() {
        if (!running) return
        Log.w(TAG, "Проекция остановлена: чищу захват, сервис жив, жду нового разрешения")
        projectionAlive = false
        stopAudio()
        stopCaptureResources()
        link?.sendControl(Messages.status(Proto.STATUS_PROJECTION_STOPPED, "projection stopped").encode())
        showReGrantNotification()
    }

    private fun stopCaptureResources() {
        try {
            if (displayListenerRegistered) displayManager.unregisterDisplayListener(displayListener)
        } catch (_: Exception) {
        }
        displayListenerRegistered = false
        try {
            pendingImage?.close()
        } catch (_: Exception) {
        }
        pendingImage = null
        trailingScheduled = false
        try {
            imageReader?.setOnImageAvailableListener(null, null)
        } catch (_: Exception) {
        }
        try {
            imageReader?.close()
        } catch (_: Exception) {
        }
        imageReader = null
        try {
            virtualDisplay?.release()
        } catch (_: Exception) {
        }
        virtualDisplay = null
        try {
            frameBitmap?.recycle()
        } catch (_: Exception) {
        }
        frameBitmap = null
        lastJpeg = null
    }

    /** Новый токен проекции для живого сервиса (перевыдача после onStop): пересоздаём захват. */
    private fun restartProjection(resultCode: Int, data: Intent) {
        Log.d(TAG, "restartProjection: пересоздаю захват на новом токене")
        try {
            val projectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            mediaProjection?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось остановить старую проекцию: $e")
        }
        mediaProjection = null
        stopCaptureResources()
        stopAudio()
        try {
            val projectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            mediaProjection = projectionManager.getMediaProjection(resultCode, data)
            Log.d(TAG, "getMediaProjection (restart) -> $mediaProjection")
        } catch (e: Exception) {
            Log.e(TAG, "Не удалось получить проекцию", e)
            return
        }
        // Пересоздаём захват на существующем потоке handler (startCapture делает новый поток,
        // здесь поток жив — создаём только дисплей и читалку).
        val projection = mediaProjection ?: return
        val h = handler
        val thread = captureThread
        if (h == null || thread == null || !thread.isAlive) {
            Log.w(TAG, "Поток захвата мёртв, пересоздаю полностью через startCapture")
            startCapture()
            return
        }
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.w(TAG, "MediaProjection.onStop (после рестарта)")
                handler?.post { onProjectionStopped() }
            }
        }, h)
        val l = computeLayout()
        layout = l
        val reader = newReader(l)
        imageReader = reader
        val vd = projection.createVirtualDisplay(
            "ScreenCapture",
            l.streamW, l.streamH, l.dpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            reader.surface,
            object : VirtualDisplay.Callback() {
                override fun onPaused() {
                    Log.d(TAG, "VirtualDisplay.onPaused")
                }

                override fun onResumed() {
                    Log.d(TAG, "VirtualDisplay.onResumed")
                }

                override fun onStopped() {
                    Log.w(TAG, "VirtualDisplay.onStopped")
                }
            },
            h
        )
        virtualDisplay = vd
        if (vd == null) {
            Log.e(TAG, "VirtualDisplay не создан после рестарта")
            return
        }
        if (!displayListenerRegistered) {
            try {
                displayManager.registerDisplayListener(displayListener, h)
                displayListenerRegistered = true
            } catch (e: Exception) {
                Log.w(TAG, "Не удалось подписаться на поворот после рестарта: $e")
            }
        }
        projectionAlive = true
        showRunningNotification()
        Log.d(TAG, "Захват перезапущен после нового разрешения")
    }

    /** Экран повернулся или изменились параметры потока: меняем размер виртуального экрана. */
    private fun relayout(reason: String) {
        try {
            relayoutUnsafe(reason)
        } catch (e: Throwable) {
            Log.e(TAG, "Сбой relayout($reason), захват продолжается как был", e)
        }
    }

    private fun relayoutUnsafe(reason: String) {
        val old = layout ?: return
        val fresh = computeLayout()
        if (fresh.sameAs(old)) return
        val vd = virtualDisplay ?: return
        Log.d(
            TAG, "Новая раскладка ($reason): экран ${fresh.screenW}x${fresh.screenH} rot=${fresh.rotation} " +
                    "-> поток ${fresh.streamW}x${fresh.streamH} (было ${old.streamW}x${old.streamH}, rot=${old.rotation})"
        )
        layout = fresh
        if (fresh.streamW != old.streamW || fresh.streamH != old.streamH || fresh.dpi != old.dpi) {
            pendingImage?.close()
            pendingImage = null
            val oldReader = imageReader
            val reader = newReader(fresh)
            vd.resize(fresh.streamW, fresh.streamH, fresh.dpi)
            vd.surface = reader.surface
            imageReader = reader
            oldReader?.setOnImageAvailableListener(null, null)
            oldReader?.close()
            frameBitmap?.recycle()
            frameBitmap = null
            lastJpeg = null // кадр прежнего размера отправлять нельзя
        }
        sendDisplayInfo()
    }

    private fun sendDisplayInfo() {
        val l = layout ?: return
        val link = link ?: return
        if (sessionActive) {
            link.sendControl(
                Messages.displayInfo(l.streamW, l.streamH, l.screenW, l.screenH, l.dpi, l.rotation).encode()
            )
        }
    }

    /** Экран по требованию с ПК (кнопка «Разбудить»): включает экран на минуту, чтобы ввести PIN.
     * Если экран уже включён — ничего не делаем (иначе кнопка мешала бы телефону заснуть).
     * Вызывать только из потока handler. */
    private fun acquireTimedWakeLock(timeoutMs: Long = 60_000) {
        releaseDockWakeLock()
        try {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            if (pm.isInteractive) {
                Log.d(TAG, "Экран уже включён, будить нечего")
                return
            }
            @Suppress("DEPRECATION")
            val wl = pm.newWakeLock(
                PowerManager.SCREEN_BRIGHT_WAKE_LOCK or PowerManager.ACQUIRE_CAUSES_WAKEUP,
                "DeviceMedia:wake"
            )
            wl.acquire(timeoutMs)
            dockWakeLock = wl
            Log.d(TAG, "Экран включён на ${timeoutMs / 1000}с по запросу с ПК")
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось включить экран: $e")
        }
    }

    private fun releaseDockWakeLock() {
        try {
            if (dockWakeLock?.isHeld == true) {
                dockWakeLock?.release()
                Log.d(TAG, "WakeLock отпущен: экран снова гаснет как обычно")
            }
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось отпустить WakeLock: $e")
        }
        dockWakeLock = null
    }

    // ---------- звук ----------

    private fun canDeliverAudio(): Boolean =
        link != null && sessionActive && stream.audio && audioActive

    private fun startAudio() {
        if (audioActive) return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            link?.sendControl(Messages.status(Proto.STATUS_AUDIO_UNAVAILABLE, "audio requires Android 10+").encode())
            return
        }
        val projection = mediaProjection ?: run {
            link?.sendControl(Messages.status(Proto.STATUS_AUDIO_UNAVAILABLE, "no projection").encode())
            return
        }
        val mgr = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        audioManager = mgr
        // Глушим локальный вывод, как будто мы внешняя колонка. Захват идёт до громкости,
        // поэтому данные продолжают идти, а из динамика тихо. Громкость вернём при остановке.
        try {
            prevMusicVolume = mgr.getStreamVolume(AudioManager.STREAM_MUSIC)
            mgr.setStreamVolume(AudioManager.STREAM_MUSIC, 0, 0)
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось приглушить музыку: $e")
            prevMusicVolume = -1
        }
        val cap = AudioCapture(projection, object : AudioCapture.Listener {
            override fun onPcm(data: ByteArray, ptsUs: Long) {
                val l = link ?: return
                if (!canDeliverAudio()) return
                l.offerAudio(Messages.audioFrame(data, ptsUs).encode())
                audioFramesSent++
            }

            override fun onError(message: String) {
                Log.w(TAG, "Звук: $message")
                handler?.post {
                    link?.sendControl(Messages.status(Proto.STATUS_AUDIO_UNAVAILABLE, message).encode())
                    stopAudio(restoreVolume = true)
                    stream.audio = false
                }
            }
        })
        if (!cap.start()) {
            stopAudio(restoreVolume = true)
            stream.audio = false
            return
        }
        audioCapture = cap
        audioActive = true
        link?.sendControl(Messages.audioConfig(AudioCapture.SAMPLE_RATE, AudioCapture.CHANNELS).encode())
        Log.d(TAG, "Звук включён: PCM ${AudioCapture.SAMPLE_RATE} Гц стерео, музыка приглушена (было $prevMusicVolume)")
    }

    private fun stopAudio(restoreVolume: Boolean = true) {
        audioCapture?.stop()
        audioCapture = null
        audioActive = false
        if (restoreVolume && prevMusicVolume >= 0) {
            try {
                (audioManager ?: getSystemService(Context.AUDIO_SERVICE) as AudioManager)
                    .setStreamVolume(AudioManager.STREAM_MUSIC, prevMusicVolume, 0)
                Log.d(TAG, "Звук выключен: громкость музыки возвращена ($prevMusicVolume)")
            } catch (e: Exception) {
                Log.w(TAG, "Не удалось вернуть громкость: $e")
            }
        }
        prevMusicVolume = -1
    }

    // ---------- кадры ----------

    private fun canDeliver(): Boolean =
        link != null && sessionActive && stream.video

    private fun onImage(reader: ImageReader) {
        try {
            onImageUnsafe(reader)
        } catch (e: Throwable) {
            // Исключение здесь убило бы поток захвата целиком (мёртвый захват при живом USB).
            Log.e(TAG, "Сбой onImage, захват продолжается", e)
        }
    }

    private fun onImageUnsafe(reader: ImageReader) {
        imageCallbacks++
        val image = reader.acquireLatestImage() ?: return
        val now = SystemClock.elapsedRealtime()
        val minInterval = when {
            !canDeliver() -> IDLE_FRAME_INTERVAL_MS // без ПК не тратим CPU: обновляем последний кадр раз в секунду
            stream.maxFps > 0 -> 1000L / stream.maxFps
            else -> 0L
        }
        val wait = lastConvertMs + minInterval - now
        if (wait > 0) {
            // Слишком рано: оставляем кадр и обработаем его позже, чтобы последнее состояние экрана не потерялось
            framesThrottled++
            pendingImage?.close()
            pendingImage = image
            if (!trailingScheduled) {
                trailingScheduled = true
                handler?.postDelayed({
                    trailingScheduled = false
                    val img = pendingImage
                    pendingImage = null
                    if (img != null) processImage(img)
                }, wait)
            }
            return
        }
        pendingImage?.close()
        pendingImage = null
        processImage(image)
    }

    private fun processImage(image: Image) {
        try {
            lastConvertMs = SystemClock.elapsedRealtime()
            val width = image.width
            val height = image.height
            val plane = image.planes[0]
            val pixelStride = plane.pixelStride
            val rowPadding = plane.rowStride - pixelStride * width
            val paddedWidth = width + rowPadding / pixelStride

            var bitmap = frameBitmap
            if (bitmap == null || bitmap.width != paddedWidth || bitmap.height != height) {
                bitmap?.recycle()
                bitmap = Bitmap.createBitmap(paddedWidth, height, Bitmap.Config.ARGB_8888)
                frameBitmap = bitmap
            }
            bitmap.copyPixelsFromBuffer(plane.buffer)
            val frame = if (rowPadding == 0) bitmap else Bitmap.createBitmap(bitmap, 0, 0, width, height)
            jpegBuffer.reset()
            frame.compress(Bitmap.CompressFormat.JPEG, stream.jpegQuality.coerceIn(1, 100), jpegBuffer)
            if (frame !== bitmap) frame.recycle()

            val jpeg = jpegBuffer.toByteArray()
            lastJpeg = jpeg
            lastPtsUs = SystemClock.elapsedRealtimeNanos() / 1000
            if (canDeliver()) deliverFrame(jpeg, lastPtsUs)
        } catch (e: Throwable) {
            Log.e(TAG, "Ошибка обработки кадра", e)
        } finally {
            image.close()
        }
    }

    private fun deliverFrame(jpeg: ByteArray, ptsUs: Long) {
        val l = link ?: return
        l.offerVideo(Messages.videoFrame(jpeg, ptsUs).encode())
        framesSent++
    }

    // ---------- USB ----------

    private fun tick() {
        val now = SystemClock.elapsedRealtime()
        if (link == null) {
            tryConnect()
            return
        }
        if (sessionActive && now - lastHostRx >= HOST_SILENT_MS) {
            sessionActive = false
            Log.w(TAG, "ПК молчит ${HOST_SILENT_MS}мс: сессия приостановлена, жду HELLO")
        }
    }

    private fun tryConnect() {
        if (link != null) return
        val accessory = usbManager.accessoryList?.firstOrNull()
        if (accessory == null) {
            permissionRequested = false
            return
        }
        if (!usbManager.hasPermission(accessory)) {
            if (!permissionRequested) {
                permissionRequested = true
                Log.d(TAG, "Аксессуар ${accessory.manufacturer} ${accessory.model} найден, запрашиваю USB-разрешение")
                val intent = Intent(ACTION_USB_PERMISSION).setPackage(packageName)
                // MUTABLE нужен, чтобы система дописала в интент результат разрешения.
                // На Android 14+ для неявного интента с MUTABLE обязателен ALLOW_UNSAFE_IMPLICIT_INTENT
                val flags = when {
                    Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE ->
                        PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_ALLOW_UNSAFE_IMPLICIT_INTENT

                    Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> PendingIntent.FLAG_MUTABLE
                    else -> 0
                }
                usbManager.requestPermission(accessory, PendingIntent.getBroadcast(this, 0, intent, flags))
            }
            return
        }
        val descriptor = try {
            usbManager.openAccessory(accessory)
        } catch (e: Exception) {
            Log.e(TAG, "openAccessory бросил исключение", e)
            null
        }
        if (descriptor == null) {
            Log.w(TAG, "openAccessory вернул null, повторю при следующем опросе")
            return
        }
        connects++
        val l = Link(descriptor, this)
        link = l
        sessionActive = false
        hostWantsForeground = false
        hostWantsLaunch = false
        hostWantsApps = false
        hostWantsUi = false
        hostWantsExit = false
        hostWantsIp = false
        hostWantsHttp = false
        hostWantsText = false
        stopUiChanges()
        stream = StreamConfig()
        Log.d(TAG, "USB подключён (#$connects): ${accessory.manufacturer} ${accessory.model}, жду HELLO от ПК")
        l.start()
    }

    // HostLink.Listener (вызывается из потоков каналов)
    override fun onPacket(packet: Packet) {
        handler?.post { handlePacket(packet) }
    }

    /** HID-команда из ADB-канала: перевод во ввод — следующим шагом (accessibility-мост). */
    override fun onHid(cmd: HidCommand) {
        Log.d(TAG, "HID по ADB: request=${cmd.request} id=${cmd.hidId} ${cmd.payload.size}Б (мост ввода — todo)")
    }

    override fun onClosed(reason: String) {
        handler?.post {
            if (link?.isClosed == true) {
                Log.w(TAG, "USB отключён: $reason (кадров отправлено=$framesSent)")
                stopAudio()
                releaseDockWakeLock()
                link = null
                sessionActive = false
                hostWantsForeground = false
                hostWantsLaunch = false
                hostWantsApps = false
                hostWantsUi = false
                hostWantsExit = false
                hostWantsIp = false
                hostWantsHttp = false
                hostWantsText = false
                stopUiChanges()
            }
        }
    }

    private fun handlePacket(p: Packet) {
        try {
            handlePacketUnsafe(p)
        } catch (e: Throwable) {
            // Исключение здесь убило бы поток захвата целиком (мёртвый захват при живом USB).
            Log.e(TAG, "Сбой handlePacket type=${p.type}, захват продолжается", e)
        }
    }

    /** Любой пакет от живого хоста снимает «паузу по тишине» без сброса настроек (в отличие от HELLO). */
    private fun reactivateIfPaused(reason: String) {
        if (!sessionActive) {
            sessionActive = true
            Log.w(TAG, "Сессия возобновлена по $reason (пауза по тишине снята)")
            lastJpeg?.let { if (canDeliver()) deliverFrame(it, lastPtsUs) }
        }
    }

    private fun handlePacketUnsafe(p: Packet) {
        val l = link ?: return
        lastHostRx = SystemClock.elapsedRealtime()
        when (p.type) {
            Proto.T_HELLO -> onHostHello(l, p)
            Proto.T_PING -> {
                reactivateIfPaused("PING")
                l.sendControl(Messages.pong(p.payload).encode())
            }

            Proto.T_SET_STREAM -> {
                val s = Messages.parseSetStream(p.payload) ?: return
                reactivateIfPaused("SET_STREAM")
                Log.d(
                    TAG, "SET_STREAM: видео=${s.video} звук=${s.audio} макс=${s.maxWidth}x${s.maxHeight} " +
                            "качество=${s.jpegQuality} fps=${s.maxFps}"
                )
                stream.video = s.video
                stream.audio = s.audio
                stream.maxWidth = s.maxWidth
                stream.maxHeight = s.maxHeight
                stream.jpegQuality = if (s.jpegQuality == 0) DEFAULT_JPEG_QUALITY else s.jpegQuality
                stream.maxFps = s.maxFps
                relayout("SET_STREAM")
                sendDisplayInfo() // телефон обязан сообщить итоговый размер, даже если он не изменился
                if (s.audio) startAudio() else stopAudio()
                lastJpeg?.let { if (canDeliver()) deliverFrame(it, lastPtsUs) }
            }

            Proto.T_REQUEST_FRAME -> {
                reactivateIfPaused("REQUEST_FRAME")
                lastJpeg?.let { if (canDeliver()) deliverFrame(it, lastPtsUs) }
            }

            Proto.T_WAKE -> {
                reactivateIfPaused("WAKE")
                acquireTimedWakeLock()
            }

            Proto.T_LAUNCH_APP -> {
                reactivateIfPaused("LAUNCH_APP")
                val cmd = Messages.parseLaunchApp(p.payload) ?: return
                // Хост не просил эту возможность в HELLO — команду не выполняем (ответ всё равно даём)
                val result = if (hostWantsLaunch) AppLauncher.launch(this, cmd.packageName) else AppLauncher.ERROR
                Log.d(TAG, "LAUNCH_APP #${cmd.requestId}: ${cmd.packageName.take(255)} → $result")
                l.sendControl(Messages.status(Proto.STATUS_LAUNCH_RESULT, "${cmd.requestId} $result").encode())
                // Передний план — вскоре после запуска: хост ждёт смены приложения
                if (result == AppLauncher.OK) handler?.postDelayed({
                    try {
                        if (running) reportForeground(false)
                    } catch (e: Throwable) {
                        Log.e(TAG, "Сбой отчёта переднего плана после LAUNCH_APP", e)
                    }
                }, 300)
            }

            Proto.T_LIST_APPS -> {
                reactivateIfPaused("LIST_APPS")
                val id = Messages.parseListApps(p.payload) ?: return
                if (!hostWantsApps) {
                    l.sendControl(Messages.status(Proto.STATUS_APP_LIST, "$id []").encode())
                    return
                }
                // Подписи и значки грузятся заметное время — не в потоке захвата
                Thread({
                    val json = try {
                        AppLauncher.listApps(this)
                    } catch (e: Throwable) {
                        Log.w(TAG, "LIST_APPS: $e")
                        "[]"
                    }
                    l.sendControl(Messages.status(Proto.STATUS_APP_LIST, "$id $json").encode())
                }, "list-apps").start()
            }

            Proto.T_UI_DUMP -> {
                reactivateIfPaused("UI_DUMP")
                val id = Messages.parseUiDump(p.payload) ?: return
                if (!hostWantsUi) {
                    l.sendControl(Messages.status(Proto.STATUS_UI_DUMP, "$id 1/1 error not_negotiated").encode())
                    return
                }
                uiExecutor.execute {
                    val answer = try {
                        UiTreeService.dump(this)
                    } catch (e: Throwable) {
                        Log.w(TAG, "UI_DUMP: $e")
                        "error " + e.javaClass.simpleName
                    }
                    val parts = UiTreeService.chunks(answer)
                    for ((i, part) in parts.withIndex()) {
                        l.sendControl(Messages.status(Proto.STATUS_UI_DUMP, "$id ${i + 1}/${parts.size} $part").encode())
                    }
                }
            }

            Proto.T_EXIT_INFO -> {
                reactivateIfPaused("EXIT_INFO")
                val cmd = Messages.parseExitInfo(p.payload) ?: return
                if (!hostWantsExit) {
                    l.sendControl(Messages.status(Proto.STATUS_EXIT_INFO, "${cmd.requestId} ${ExitInfo.UNSUPPORTED}").encode())
                    return
                }
                uiExecutor.execute {
                    val answer = try {
                        ExitInfo.query(this, cmd.packageName, cmd.sinceMs)
                    } catch (e: Throwable) {
                        Log.w(TAG, "EXIT_INFO: $e")
                        "[]"
                    }
                    l.sendControl(Messages.status(Proto.STATUS_EXIT_INFO, "${cmd.requestId} $answer").encode())
                }
            }

            Proto.T_IP_CHECK -> {
                reactivateIfPaused("IP_CHECK")
                val cmd = Messages.parseIpCheck(p.payload) ?: return
                if (!hostWantsIp) {
                    l.sendControl(Messages.status(Proto.STATUS_IP_CHECK, "${cmd.requestId} ${IpCheck.error("not_negotiated", "none", 0)}").encode())
                    return
                }
                // Сеть — не в потоке захвата и не в очереди дампов экрана
                ipExecutor.execute {
                    val answer = try {
                        IpCheck.run(this, cmd.url)
                    } catch (e: Throwable) {
                        IpCheck.error("io: " + e.javaClass.simpleName, "none", 0)
                    }
                    Log.d(TAG, "IP_CHECK #${cmd.requestId}: $answer")
                    l.sendControl(Messages.status(Proto.STATUS_IP_CHECK, "${cmd.requestId} $answer").encode())
                }
            }

            Proto.T_HTTP_LOCAL -> {
                reactivateIfPaused("HTTP_LOCAL")
                val cmd = Messages.parseHttpLocal(p.payload) ?: return
                if (!hostWantsHttp) {
                    l.sendControl(Messages.status(Proto.STATUS_HTTP_LOCAL, "${cmd.requestId} ${HttpLocal.error("not_negotiated", 0)}").encode())
                    return
                }
                // Сеть — не в потоке захвата
                httpExecutor.execute {
                    val answer = try {
                        HttpLocal.run(this, cmd.json)
                    } catch (e: Throwable) {
                        HttpLocal.error("io: " + e.javaClass.simpleName, 0)
                    }
                    Log.d(TAG, "HTTP_LOCAL #${cmd.requestId}: ${answer.take(200)}")
                    l.sendControl(Messages.status(Proto.STATUS_HTTP_LOCAL, "${cmd.requestId} $answer").encode())
                }
            }

            Proto.T_TEXT_INPUT -> {
                reactivateIfPaused("TEXT_INPUT")
                val cmd = Messages.parseTextInput(p.payload) ?: return
                if (!hostWantsText) {
                    l.sendControl(Messages.status(Proto.STATUS_TEXT_INPUT, "${cmd.requestId} ${TextInput.error("not_negotiated", 0)}").encode())
                    return
                }
                // Буфер обмена — в главном потоке, действия службы — межпроцессные вызовы: не в потоке захвата
                textExecutor.execute {
                    val answer = try {
                        TextInput.run(this, cmd.bytes)
                    } catch (e: Throwable) {
                        TextInput.error("error: " + e.javaClass.simpleName, 0)
                    }
                    // Сам текст в журнал не пишется
                    Log.d(TAG, "TEXT_INPUT #${cmd.requestId} (${cmd.bytes.size} байт): $answer")
                    l.sendControl(Messages.status(Proto.STATUS_TEXT_INPUT, "${cmd.requestId} $answer").encode())
                }
            }

            else -> {} // PONG и неизвестные типы пропускаем
        }
    }

    private fun onHostHello(l: HostLink, p: Packet) {
        val hello = Messages.parseHello(p.payload)
        if (hello == null || hello.role != 0) return
        val versionMax = minOf(Proto.VERSION, hello.versionMax)
        val versionMin = maxOf(Proto.VERSION, hello.versionMin)
        if (versionMax < versionMin) {
            Log.w(TAG, "Версия ПК ${hello.versionMin}..${hello.versionMax} несовместима с ${Proto.VERSION}")
            l.sendControl(Messages.error(Proto.ERR_UNSUPPORTED_VERSION, "supported version: ${Proto.VERSION}").encode())
            return
        }
        Log.d(TAG, "HELLO от ПК: ${hello.name}, версия $versionMax, возможности ${hello.capabilities}")
        // Любой HELLO перезапускает сессию: настройки потока сбрасываются
        stopAudio()
        sessionActive = true
        stream = StreamConfig()
        relayout("HELLO")
        var caps = Proto.CAP_VIDEO_JPEG or Proto.CAP_ROTATION or Proto.CAP_FOREGROUND or Proto.CAP_LAUNCH_APP or Proto.CAP_LIST_APPS or
            Proto.CAP_UI_TREE or Proto.CAP_EXIT_INFO or Proto.CAP_IP_CHECK or Proto.CAP_HTTP_LOCAL or Proto.CAP_TEXT_INPUT
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) caps = caps or Proto.CAP_AUDIO_PCM
        hostWantsForeground = hello.capabilities and Proto.CAP_FOREGROUND != 0
        hostWantsLaunch = hello.capabilities and Proto.CAP_LAUNCH_APP != 0
        hostWantsApps = hello.capabilities and Proto.CAP_LIST_APPS != 0
        hostWantsUi = hello.capabilities and Proto.CAP_UI_TREE != 0
        hostWantsExit = hello.capabilities and Proto.CAP_EXIT_INFO != 0
        hostWantsIp = hello.capabilities and Proto.CAP_IP_CHECK != 0
        hostWantsHttp = hello.capabilities and Proto.CAP_HTTP_LOCAL != 0
        hostWantsText = hello.capabilities and Proto.CAP_TEXT_INPUT != 0
        if (hostWantsUi) startUiChanges(l) else stopUiChanges()
        l.sendControl(Messages.hello(caps, NAME).encode())
        val lay = layout
        if (lay != null) {
            l.sendControl(
                Messages.displayInfo(
                    lay.streamW,
                    lay.streamH,
                    lay.screenW,
                    lay.screenH,
                    lay.dpi,
                    lay.rotation
                ).encode()
            )
        }
        lastJpeg?.let { deliverFrame(it, lastPtsUs) }
        reportForeground(true)
    }

    /** «Экран изменился» → хосту (STATUS 21): служба элементов экрана зовёт не чаще 4 раз в секунду */
    private fun startUiChanges(l: HostLink) {
        UiTreeService.onChange = {
            if (link === l && sessionActive && hostWantsUi) l.sendControl(Messages.status(Proto.STATUS_UI_CHANGED, "").encode())
        }
    }

    private fun stopUiChanges() {
        UiTreeService.onChange = null
    }

    /**
     * Пакет на переднем плане → хосту (STATUS 16), если хост попросил (CAP_FOREGROUND в его HELLO).
     * Без «Доступа к истории использования» — STATUS 17 "usage_access": хост покажет, что разрешить.
     * force — сразу (после HELLO), иначе при смене пакета и раз в секунду (хост по этому видит свежесть).
     */
    private fun reportForeground(force: Boolean) {
        val l = link ?: return
        if (!sessionActive || !hostWantsForeground) return
        val watch = foregroundWatch ?: ForegroundWatch(this).also { foregroundWatch = it }
        val now = SystemClock.elapsedRealtime()
        if (!watch.granted()) {
            if (force || foregroundSent != null || now - foregroundSentAt >= FOREGROUND_REPEAT_MS) {
                l.sendControl(Messages.status(Proto.STATUS_FOREGROUND_UNAVAILABLE, "usage_access").encode())
                foregroundSent = null
                foregroundSentAt = now
            }
            return
        }
        val pkg = watch.poll() ?: ""
        if (force || pkg != foregroundSent || now - foregroundSentAt >= FOREGROUND_REPEAT_MS) {
            l.sendControl(Messages.status(Proto.STATUS_FOREGROUND, pkg).encode())
            foregroundSent = pkg
            foregroundSentAt = now
        }
    }

    private fun logHeartbeat() {
        val sec = (SystemClock.elapsedRealtime() - startedAt) / 1000
        val l = link
        val lay = layout
        val frameAge = (SystemClock.elapsedRealtime() - lastConvertMs) / 1000
        val locked = try {
            val km = getSystemService(Context.KEYGUARD_SERVICE) as android.app.KeyguardManager
            if (km.isDeviceLocked) "да" else "нет"
        } catch (_: Exception) {
            "?"
        }
        Log.d(
            TAG, "[hb ${sec}s] usb=${if (l != null) "подключён" else "нет"} сессия=$sessionActive " +
                    "проекция=${if (projectionAlive) "жива" else "МЕРТВА"} кадр_назад=${frameAge}с locked=${locked} " +
                    "подключений=$connects колбэков=$imageCallbacks отправлено=$framesSent придержано=$framesThrottled " +
                    (if (l != null) "записано=${l.packetsWritten}пак/${l.bytesWritten}Б прочитано=${l.bytesRead}Б " +
                            "отброшено_видео=${l.videoDropped} отброшено_звук=${l.audioDropped} макс_запись=${l.maxWriteMs}мс " else "") +
                    "поток=${lay?.streamW}x${lay?.streamH} rot=${lay?.rotation} звук=${if (audioActive) "вкл(${audioFramesSent})" else "выкл"}"
        )
    }

    private fun buildNotification(text: String, reGrant: Boolean): android.app.Notification {
        val stopIntent = PendingIntent.getService(
            this, 1,
            Intent(this, ScreenCaptureService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val b = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("Док-станция")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_menu_gallery)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setOngoing(true)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Остановить", stopIntent)
        if (reGrant) {
            val grantIntent = PendingIntent.getActivity(
                this, 2,
                Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            b.addAction(android.R.drawable.ic_menu_gallery, "Разрешить снова", grantIntent)
            b.setContentIntent(grantIntent)
        }
        return b.build()
    }

    private fun showRunningNotification() {
        try {
            getSystemService(NotificationManager::class.java)
                .notify(1, buildNotification("Экран телефона передаётся на ПК", false))
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось обновить уведомление: $e")
        }
    }

    private fun showReGrantNotification() {
        try {
            getSystemService(NotificationManager::class.java)
                .notify(1, buildNotification("Запись экрана остановлена — нажмите, чтобы разрешить снова", true))
        } catch (e: Exception) {
            Log.w(TAG, "Не удалось обновить уведомление: $e")
        }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(CHANNEL_ID, "Док-станция", NotificationManager.IMPORTANCE_LOW)
                .apply { description = "Состояние передачи экрана на ПК" }
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    override fun onDestroy() {
        Log.d(TAG, "Service onDestroy: кадров отправлено=$framesSent, подключений=$connects")
        stopUiChanges()
        uiExecutor.shutdownNow()
        ipExecutor.shutdownNow()
        httpExecutor.shutdownNow()
        textExecutor.shutdownNow()
        running = false
        isRunning = false
        projectionAlive = false
        stopAudio()
        releaseDockWakeLock()
        if (displayListenerRegistered) displayManager.unregisterDisplayListener(displayListener)
        if (receiverRegistered) {
            try {
                unregisterReceiver(usbReceiver)
            } catch (_: IllegalArgumentException) {
            }
        }
        link?.close("service destroyed")
        pendingImage?.close()
        imageReader?.setOnImageAvailableListener(null, null)
        virtualDisplay?.release()
        imageReader?.close()
        mediaProjection?.stop()
        captureThread?.quitSafely()
        frameBitmap?.recycle()
        super.onDestroy()
    }
}
