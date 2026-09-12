//! The platform services that only exist on the Java side: the clipboard,
//! window insets, intents, the display's refresh rate and the keystore.
//! Each is one short chain of JNI calls against the activity; a failure or
//! a Java exception is reported as an error and the caller falls back.

use android_activity::AndroidApp;
use anyhow::{Context as _, Result, anyhow};
use jni::{
    Env, JavaVM, jni_sig, jni_str,
    objects::{JByteArray, JObject, JString, JValue},
    refs::Global,
    sys::jobject,
};

/// Runs `f` with the current thread attached to the JVM and the activity
/// object. A Java exception left pending by `f` is cleared and reported.
pub(crate) fn with_activity<R>(
    app: &AndroidApp,
    f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<R>,
) -> Result<R> {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut jni::sys::JavaVM) };
    let activity: jobject = app.activity_as_ptr() as jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<R> {
        let activity = unsafe { env.as_cast_raw::<Global<JObject<'static>>>(&activity)? };
        let result = f(env, &activity);
        if env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        result
    })
    .map_err(|error: jni::errors::Error| anyhow!("JNI call failed: {error}"))
}

fn string_of(env: &mut Env<'_>, object: &JObject<'_>) -> jni::errors::Result<String> {
    if object.is_null() {
        return Ok(String::new());
    }
    let string = env.as_cast::<JString>(object)?;
    string.try_to_string(env)
}

fn system_service<'local>(
    env: &mut Env<'local>,
    activity: &JObject<'_>,
    name: &str,
) -> jni::errors::Result<JObject<'local>> {
    let name = env.new_string(name)?;
    env.call_method(
        activity,
        jni_str!("getSystemService"),
        jni_sig!((name: JString) -> JObject),
        &[JValue::Object(&name)],
    )?
    .l()
}

/// The text on the clipboard, if any. Android only lets the focused app
/// read the clipboard, so this is `None` while the app is in the
/// background.
pub(crate) fn clipboard_text(app: &AndroidApp) -> Result<Option<String>> {
    with_activity(app, |env, activity| {
        let manager = system_service(env, activity, "clipboard")?;
        let has_clip = env
            .call_method(
                &manager,
                jni_str!("hasPrimaryClip"),
                jni_sig!(() -> bool),
                &[],
            )?
            .z()?;
        if !has_clip {
            return Ok(None);
        }
        let clip = env
            .call_method(
                &manager,
                jni_str!("getPrimaryClip"),
                jni_sig!(() -> android.content.ClipData),
                &[],
            )?
            .l()?;
        if clip.is_null() {
            return Ok(None);
        }
        let count = env
            .call_method(&clip, jni_str!("getItemCount"), jni_sig!(() -> int), &[])?
            .i()?;
        if count == 0 {
            return Ok(None);
        }
        let item = env
            .call_method(
                &clip,
                jni_str!("getItemAt"),
                jni_sig!((index: int) -> android.content.ClipData::Item),
                &[JValue::Int(0)],
            )?
            .l()?;
        let text = env
            .call_method(
                &item,
                jni_str!("coerceToText"),
                jni_sig!((context: android.content.Context) -> java.lang.CharSequence),
                &[JValue::Object(activity)],
            )?
            .l()?;
        if text.is_null() {
            return Ok(None);
        }
        let text = env
            .call_method(&text, jni_str!("toString"), jni_sig!(() -> JString), &[])?
            .l()?;
        Ok(Some(string_of(env, &text)?))
    })
}

pub(crate) fn set_clipboard_text(app: &AndroidApp, text: &str) -> Result<()> {
    with_activity(app, |env, activity| {
        let manager = system_service(env, activity, "clipboard")?;
        let label = env.new_string("gpui")?;
        let text = env.new_string(text)?;
        let clip = env
            .call_static_method(
                jni_str!("android/content/ClipData"),
                jni_str!("newPlainText"),
                jni_sig!((label: java.lang.CharSequence, text: java.lang.CharSequence) -> android.content.ClipData),
                &[JValue::Object(&label), JValue::Object(&text)],
            )?
            .l()?;
        env.call_method(
            &manager,
            jni_str!("setPrimaryClip"),
            jni_sig!((clip: android.content.ClipData)),
            &[JValue::Object(&clip)],
        )?;
        Ok(())
    })
}

/// Starts an `ACTION_VIEW` intent for `url`: a browser for http(s), the
/// registered app for anything else.
pub(crate) fn open_url(app: &AndroidApp, url: &str) -> Result<()> {
    /// `Intent.FLAG_ACTIVITY_NEW_TASK`.
    const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
    with_activity(app, |env, activity| {
        let action = env.new_string("android.intent.action.VIEW")?;
        let url = env.new_string(url)?;
        let uri = env
            .call_static_method(
                jni_str!("android/net/Uri"),
                jni_str!("parse"),
                jni_sig!((url: JString) -> android.net.Uri),
                &[JValue::Object(&url)],
            )?
            .l()?;
        let intent = env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!((action: JString, uri: android.net.Uri)),
            &[JValue::Object(&action), JValue::Object(&uri)],
        )?;
        env.call_method(
            &intent,
            jni_str!("addFlags"),
            jni_sig!((flags: int) -> android.content.Intent),
            &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)],
        )?;
        env.call_method(
            activity,
            jni_str!("startActivity"),
            jni_sig!((intent: android.content.Intent)),
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// Finishes the activity, which is how an Android app leaves the screen.
pub(crate) fn finish_activity(app: &AndroidApp) -> Result<()> {
    with_activity(app, |env, activity| {
        env.call_method(activity, jni_str!("finish"), jni_sig!(()), &[])?;
        Ok(())
    })
}

/// The data URL of the intent the activity was started with, if any: the
/// file or link the app was launched to open.
pub(crate) fn launch_url(app: &AndroidApp) -> Result<Option<String>> {
    with_activity(app, |env, activity| {
        let intent = env
            .call_method(
                activity,
                jni_str!("getIntent"),
                jni_sig!(() -> android.content.Intent),
                &[],
            )?
            .l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let data = env
            .call_method(
                &intent,
                jni_str!("getDataString"),
                jni_sig!(() -> JString),
                &[],
            )?
            .l()?;
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(string_of(env, &data)?))
    })
}

/// The edges of the window covered by system UI, in physical pixels:
/// the status and navigation bars, a display cutout, and the software
/// keyboard while it is up. `None` before the window has been attached
/// (there are no insets to read) or on Android versions before 11.
pub(crate) fn window_insets(app: &AndroidApp) -> Result<Option<[i32; 4]>> {
    /// `WindowInsets.Type`: `statusBars | navigationBars | captionBar`,
    /// `ime` and `displayCutout`.
    const SYSTEM_BARS: i32 = 1 | 2 | 4;
    const IME: i32 = 8;
    const DISPLAY_CUTOUT: i32 = 128;

    if AndroidApp::sdk_version() < 30 {
        return Ok(None);
    }
    with_activity(app, |env, activity| {
        let window = env
            .call_method(
                activity,
                jni_str!("getWindow"),
                jni_sig!(() -> android.view.Window),
                &[],
            )?
            .l()?;
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!(() -> android.view.View),
                &[],
            )?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!(() -> android.view.WindowInsets),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(None);
        }
        let insets = env
            .call_method(
                &insets,
                jni_str!("getInsets"),
                jni_sig!((mask: int) -> android.graphics.Insets),
                &[JValue::Int(SYSTEM_BARS | IME | DISPLAY_CUTOUT)],
            )?
            .l()?;
        let mut edges = [0; 4];
        for (edge, name) in edges.iter_mut().zip([
            jni_str!("top"),
            jni_str!("right"),
            jni_str!("bottom"),
            jni_str!("left"),
        ]) {
            *edge = env.get_field(&insets, name, jni_sig!(int))?.i()?;
        }
        Ok(Some(edges))
    })
}

/// The character `key_code` produces under `meta` on input device
/// `device_id`, per its `KeyCharacterMap`. `-1` is the virtual keyboard,
/// which is what the software keyboard's key events report.
pub(crate) fn key_character(
    app: &AndroidApp,
    device_id: i32,
    key_code: i32,
    meta: i32,
) -> Result<Option<char>> {
    /// `KeyCharacterMap.COMBINING_ACCENT`: set on a dead key's value.
    const COMBINING_ACCENT: i32 = i32::MIN;
    with_activity(app, |env, _| {
        let map = env
            .call_static_method(
                jni_str!("android/view/KeyCharacterMap"),
                jni_str!("load"),
                jni_sig!((device_id: int) -> android.view.KeyCharacterMap),
                &[JValue::Int(device_id)],
            )?
            .l()?;
        if map.is_null() {
            return Ok(None);
        }
        let value = env
            .call_method(
                &map,
                jni_str!("get"),
                jni_sig!((key_code: int, meta: int) -> int),
                &[JValue::Int(key_code), JValue::Int(meta)],
            )?
            .i()?;
        let value = value & !COMBINING_ACCENT;
        Ok(char::from_u32(value as u32).filter(|c| *c != '\0'))
    })
}

/// The refresh rate of the display the activity is on, in Hz.
pub(crate) fn refresh_rate(app: &AndroidApp) -> Result<f32> {
    with_activity(app, |env, activity| {
        let display = env
            .call_method(
                activity,
                jni_str!("getDisplay"),
                jni_sig!(() -> android.view.Display),
                &[],
            )?
            .l()?;
        if display.is_null() {
            return Ok(60.0);
        }
        env.call_method(
            &display,
            jni_str!("getRefreshRate"),
            jni_sig!(() -> float),
            &[],
        )?
        .f()
    })
}

/// The Android keystore, holding one AES key that encrypts the app's
/// stored credentials. The key never leaves the keystore; the app is
/// handed ciphertext to keep in its private storage.
mod keystore {
    use super::*;

    const ALIAS: &str = "gpui.credentials";
    /// `KeyProperties.PURPOSE_ENCRYPT | PURPOSE_DECRYPT`.
    const PURPOSES: i32 = 1 | 2;
    /// `Cipher.ENCRYPT_MODE` / `DECRYPT_MODE`.
    const ENCRYPT_MODE: i32 = 1;
    const DECRYPT_MODE: i32 = 2;
    const GCM_TAG_BITS: i32 = 128;

    fn string_array<'local>(
        env: &mut Env<'local>,
        value: &str,
    ) -> jni::errors::Result<JObject<'local>> {
        let value = env.new_string(value)?;
        let array = env.new_object_array(1, jni_str!("java/lang/String"), &value)?;
        Ok(array.into())
    }

    /// The credentials key, generated on first use.
    fn key<'local>(env: &mut Env<'local>) -> jni::errors::Result<JObject<'local>> {
        let provider = env.new_string("AndroidKeyStore")?;
        let alias = env.new_string(ALIAS)?;
        let store = env
            .call_static_method(
                jni_str!("java/security/KeyStore"),
                jni_str!("getInstance"),
                jni_sig!((kind: JString) -> java.security.KeyStore),
                &[JValue::Object(&provider)],
            )?
            .l()?;
        env.call_method(
            &store,
            jni_str!("load"),
            jni_sig!((parameter: java.security.KeyStore::LoadStoreParameter)),
            &[JValue::Object(&JObject::null())],
        )?;
        let key = env
            .call_method(
                &store,
                jni_str!("getKey"),
                jni_sig!((alias: JString, password: [char]) -> java.security.Key),
                &[JValue::Object(&alias), JValue::Object(&JObject::null())],
            )?
            .l()?;
        if !key.is_null() {
            return Ok(key);
        }

        let algorithm = env.new_string("AES")?;
        let generator = env
            .call_static_method(
                jni_str!("javax/crypto/KeyGenerator"),
                jni_str!("getInstance"),
                jni_sig!((algorithm: JString, provider: JString) -> javax.crypto.KeyGenerator),
                &[JValue::Object(&algorithm), JValue::Object(&provider)],
            )?
            .l()?;
        let builder = env.new_object(
            jni_str!("android/security/keystore/KeyGenParameterSpec$Builder"),
            jni_sig!((alias: JString, purposes: int)),
            &[JValue::Object(&alias), JValue::Int(PURPOSES)],
        )?;
        let modes = string_array(env, "GCM")?;
        env.call_method(
            &builder,
            jni_str!("setBlockModes"),
            jni_sig!((modes: [JString]) -> android.security.keystore.KeyGenParameterSpec::Builder),
            &[JValue::Object(&modes)],
        )?;
        let paddings = string_array(env, "NoPadding")?;
        env.call_method(
            &builder,
            jni_str!("setEncryptionPaddings"),
            jni_sig!((paddings: [JString]) -> android.security.keystore.KeyGenParameterSpec::Builder),
            &[JValue::Object(&paddings)],
        )?;
        env.call_method(
            &builder,
            jni_str!("setKeySize"),
            jni_sig!((bits: int) -> android.security.keystore.KeyGenParameterSpec::Builder),
            &[JValue::Int(256)],
        )?;
        let spec = env
            .call_method(
                &builder,
                jni_str!("build"),
                jni_sig!(() -> android.security.keystore.KeyGenParameterSpec),
                &[],
            )?
            .l()?;
        env.call_method(
            &generator,
            jni_str!("init"),
            jni_sig!((spec: java.security.spec.AlgorithmParameterSpec)),
            &[JValue::Object(&spec)],
        )?;
        env.call_method(
            &generator,
            jni_str!("generateKey"),
            jni_sig!(() -> javax.crypto.SecretKey),
            &[],
        )?
        .l()
    }

    fn cipher<'local>(env: &mut Env<'local>) -> jni::errors::Result<JObject<'local>> {
        let transformation = env.new_string("AES/GCM/NoPadding")?;
        env.call_static_method(
            jni_str!("javax/crypto/Cipher"),
            jni_str!("getInstance"),
            jni_sig!((transformation: JString) -> javax.crypto.Cipher),
            &[JValue::Object(&transformation)],
        )?
        .l()
    }

    fn do_final<'local>(
        env: &mut Env<'local>,
        cipher: &JObject<'_>,
        input: &[u8],
    ) -> jni::errors::Result<Vec<u8>> {
        let input = env.byte_array_from_slice(input)?;
        let output = env
            .call_method(
                cipher,
                jni_str!("doFinal"),
                jni_sig!((input: [byte]) -> [byte]),
                &[JValue::Object(&input)],
            )?
            .l()?;
        let output = env.as_cast::<JByteArray>(&output)?;
        env.convert_byte_array(&*output)
    }

    /// Encrypts `plaintext`; the result carries the IV in front.
    pub(crate) fn encrypt(app: &AndroidApp, plaintext: &[u8]) -> Result<Vec<u8>> {
        with_activity(app, |env, _| {
            let key = key(env)?;
            let cipher = cipher(env)?;
            env.call_method(
                &cipher,
                jni_str!("init"),
                jni_sig!((mode: int, key: java.security.Key)),
                &[JValue::Int(ENCRYPT_MODE), JValue::Object(&key)],
            )?;
            let iv = env
                .call_method(&cipher, jni_str!("getIV"), jni_sig!(() -> [byte]), &[])?
                .l()?;
            let iv = env.as_cast::<JByteArray>(&iv)?;
            let iv = env.convert_byte_array(&*iv)?;
            let ciphertext = do_final(env, &cipher, plaintext)?;
            let mut out = Vec::with_capacity(1 + iv.len() + ciphertext.len());
            out.push(iv.len() as u8);
            out.extend_from_slice(&iv);
            out.extend_from_slice(&ciphertext);
            Ok(out)
        })
    }

    pub(crate) fn decrypt(app: &AndroidApp, data: &[u8]) -> Result<Vec<u8>> {
        let (&iv_len, rest) = data.split_first().context("credentials file is empty")?;
        let iv_len = iv_len as usize;
        anyhow::ensure!(rest.len() >= iv_len, "credentials file is truncated");
        let (iv, ciphertext) = rest.split_at(iv_len);
        with_activity(app, |env, _| {
            let key = key(env)?;
            let cipher = cipher(env)?;
            let iv = env.byte_array_from_slice(iv)?;
            let spec = env.new_object(
                jni_str!("javax/crypto/spec/GCMParameterSpec"),
                jni_sig!((tag_bits: int, iv: [byte])),
                &[JValue::Int(GCM_TAG_BITS), JValue::Object(&iv)],
            )?;
            env.call_method(
                &cipher,
                jni_str!("init"),
                jni_sig!((mode: int, key: java.security.Key, spec: java.security.spec.AlgorithmParameterSpec)),
                &[
                    JValue::Int(DECRYPT_MODE),
                    JValue::Object(&key),
                    JValue::Object(&spec),
                ],
            )?;
            do_final(env, &cipher, ciphertext)
        })
    }
}

pub(crate) use keystore::{decrypt, encrypt};
