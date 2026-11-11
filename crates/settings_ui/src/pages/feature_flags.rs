use std::any::{Any, TypeId, type_name};

use feature_flags::{
    AcpBetaFeatureFlag, FeatureFlag, FeatureFlagAppExt as _, FeatureFlagDescriptor,
    FeatureFlagStore, FeatureFlagValue as _, FeatureFlagVariant, PresenceFlag,
};
use fs::Fs;
use gpui::{ReadGlobal as _, ScrollHandle, Stateful, prelude::*};
use settings::{SettingsContent, SettingsStore};
use ui::{Checkbox, Switch, ToggleState, prelude::*};

use crate::{
    AnySettingField, SettingItem, SettingsFieldMetadata, SettingsUiFile, SettingsWindow,
    render_settings_item,
};

pub(crate) struct AcpBetaSettingField;

impl AnySettingField for AcpBetaSettingField {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn type_id(&self) -> TypeId {
        TypeId::of::<Self>()
    }

    fn file_set_in(&self, file: SettingsUiFile, cx: &App) -> (settings::SettingsFile, bool) {
        let (file, _) = SettingsStore::global(cx).get_value_from_file(
            file.to_settings(),
            |content: &SettingsContent| {
                content
                    .feature_flags
                    .as_ref()?
                    .get(AcpBetaFeatureFlag::NAME)
            },
        );
        // Feature flags resolve runtime defaults even without a raw JSON default.
        (file, true)
    }

    fn reset_to_default_fn(
        &self,
        current_file: &SettingsUiFile,
        file_set_in: &settings::SettingsFile,
        cx: &App,
    ) -> Option<Box<dyn Fn(&mut Window, &mut App)>> {
        if *current_file != SettingsUiFile::User
            || *file_set_in != settings::SettingsFile::User
            || FeatureFlagStore::override_for(AcpBetaFeatureFlag::NAME, cx).is_none()
        {
            return None;
        }

        Some(Box::new(|_, cx| {
            FeatureFlagStore::clear_override(AcpBetaFeatureFlag::NAME, <dyn Fs>::global(cx), cx);
        }))
    }

    fn json_path(&self) -> Option<&'static str> {
        Some("feature_flags.acp-beta")
    }

    fn is_overridden_by_organization(&self, _cx: &App) -> bool {
        false
    }
}

fn set_acp_beta_enabled(enabled: bool, cx: &App) {
    let value = if enabled {
        PresenceFlag::On
    } else {
        PresenceFlag::Off
    };
    FeatureFlagStore::set_override(
        AcpBetaFeatureFlag::NAME,
        value.override_key().to_string(),
        <dyn Fs>::global(cx),
        cx,
    );
}

pub(crate) fn render_acp_beta_setting(
    settings_window: &SettingsWindow,
    item: &SettingItem,
    settings_file: SettingsUiFile,
    _metadata: Option<&SettingsFieldMetadata>,
    sub_field: bool,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Stateful<Div> {
    let state = if cx.has_flag::<AcpBetaFeatureFlag>() {
        ToggleState::Selected
    } else {
        ToggleState::Unselected
    };
    let control = Switch::new("acp-beta", state)
        .tab_index(0_isize)
        .aria_label(item.title)
        .aria_description(item.description)
        .disabled(AcpBetaFeatureFlag::enabled_for_all())
        .on_click(|state, _, cx| {
            set_acp_beta_enabled(*state == ToggleState::Selected, cx);
        })
        .into_any_element();
    render_settings_item(settings_window, item, settings_file, control, sub_field, cx)
}

pub(crate) fn render_feature_flags_page(
    _settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    // Sort by flag name so the list is stable between renders even though
    // `inventory::iter` order depends on link order.
    let mut descriptors: Vec<&'static FeatureFlagDescriptor> =
        FeatureFlagStore::known_flags().collect();
    descriptors.sort_by_key(|descriptor| descriptor.name);

    v_flex()
        .id("feature-flags-page")
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .children(
            descriptors
                .into_iter()
                .map(|descriptor| render_flag_row(descriptor, cx)),
        )
        .into_any_element()
}

fn render_flag_row(
    descriptor: &'static FeatureFlagDescriptor,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let forced_on = FeatureFlagStore::is_forced_on(descriptor);
    let resolved = cx.global::<FeatureFlagStore>().resolved_key(descriptor, cx);
    let has_override = FeatureFlagStore::override_for(descriptor.name, cx).is_some();

    let header =
        h_flex()
            .justify_between()
            .items_center()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(descriptor.name).size(LabelSize::Default).color(
                        if forced_on {
                            Color::Muted
                        } else {
                            Color::Default
                        },
                    ))
                    .when(forced_on, |this| {
                        this.child(
                            Label::new("enabled for all")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(has_override && !forced_on, |this| {
                let name = descriptor.name;
                this.child(
                    Button::new(SharedString::from(format!("reset-{}", name)), "Reset")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            FeatureFlagStore::clear_override(name, <dyn Fs>::global(cx), cx);
                        })),
                )
            });

    v_flex()
        .id(SharedString::from(format!("flag-row-{}", descriptor.name)))
        .gap_1()
        .child(header)
        .child(render_flag_variants(descriptor, resolved, forced_on, cx))
        .into_any_element()
}

fn render_flag_variants(
    descriptor: &'static FeatureFlagDescriptor,
    resolved: &'static str,
    forced_on: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let variants: Vec<FeatureFlagVariant> = (descriptor.variants)();

    let row_items = variants.into_iter().map({
        let name = descriptor.name;
        move |variant| {
            let key = variant.override_key;
            let label = variant.label;
            let selected = resolved == key;
            let state = if selected {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            };
            let checkbox_id = SharedString::from(format!("{}-{}", name, key));
            let disabled = forced_on;
            let mut checkbox = Checkbox::new(ElementId::from(checkbox_id), state)
                .label(label)
                .disabled(disabled);
            if !disabled {
                checkbox =
                    checkbox.on_click(cx.listener(move |_, new_state: &ToggleState, _, cx| {
                        // Clicking an already-selected option is a no-op rather than a
                        // "deselect" — there's no valid "nothing selected" state.
                        if *new_state == ToggleState::Unselected {
                            return;
                        }
                        FeatureFlagStore::set_override(
                            name,
                            key.to_string(),
                            <dyn Fs>::global(cx),
                            cx,
                        );
                    }));
            }
            checkbox.into_any_element()
        }
    });

    h_flex().gap_4().flex_wrap().children(row_items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn acp_beta_persists_override_and_reset_restores_rollout(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(paths::settings_file().parent().expect("settings parent"))
            .await
            .expect("create fake settings directory");
        let initial_settings = r#"{"feature_flags":{"unrelated-flag":"custom"},"ui_font_size":17}"#;
        fs.insert_file(paths::settings_file(), initial_settings.as_bytes().to_vec())
            .await;
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store
                .set_user_settings(initial_settings, cx)
                .result()
                .expect("load settings");
            cx.set_global(store);
            <dyn Fs>::set_global(fs.clone(), cx);
            FeatureFlagStore::init(cx);
            cx.update_global::<FeatureFlagStore, _>(|store, _| {
                store.update_server_flags(false, vec![AcpBetaFeatureFlag::NAME.into()]);
            });
            assert!(cx.has_flag::<AcpBetaFeatureFlag>());
            assert!(
                AcpBetaSettingField
                    .reset_to_default_fn(
                        &SettingsUiFile::User,
                        &settings::SettingsFile::Default,
                        cx,
                    )
                    .is_none()
            );
        });

        for enabled in [true, false] {
            cx.update(|cx| set_acp_beta_enabled(enabled, cx));
            cx.run_until_parked();
            let content = fs
                .load(paths::settings_file())
                .await
                .expect("read persisted settings");
            let content: serde_json::Value =
                serde_json::from_str(&content).expect("valid settings JSON");
            assert_eq!(
                content["feature_flags"]["acp-beta"],
                if enabled { "on" } else { "off" }
            );
            assert_eq!(content["feature_flags"]["unrelated-flag"], "custom");
            assert_eq!(content["ui_font_size"], 17);
            cx.update(|cx| {
                assert_eq!(cx.has_flag::<AcpBetaFeatureFlag>(), enabled);
                assert_eq!(
                    FeatureFlagStore::override_for(AcpBetaFeatureFlag::NAME, cx),
                    Some(if enabled { "on" } else { "off" }),
                );
                assert_eq!(
                    AcpBetaSettingField.file_set_in(SettingsUiFile::User, cx),
                    (settings::SettingsFile::User, true),
                );
            });
        }

        let window = cx.add_empty_window();
        window.update(|window, cx| {
            let reset = AcpBetaSettingField
                .reset_to_default_fn(&SettingsUiFile::User, &settings::SettingsFile::User, cx)
                .expect("explicit override can be reset");
            reset(window, cx);
        });
        cx.run_until_parked();
        let content = fs
            .load(paths::settings_file())
            .await
            .expect("read reset settings");
        let content: serde_json::Value =
            serde_json::from_str(&content).expect("valid settings JSON");
        assert!(content["feature_flags"].get("acp-beta").is_none());
        assert_eq!(content["feature_flags"]["unrelated-flag"], "custom");
        assert_eq!(content["ui_font_size"], 17);
        cx.update(|cx| {
            assert_eq!(
                FeatureFlagStore::override_for(AcpBetaFeatureFlag::NAME, cx),
                None,
            );
            assert!(cx.has_flag::<AcpBetaFeatureFlag>());
            assert_eq!(
                AcpBetaSettingField.file_set_in(SettingsUiFile::User, cx),
                (settings::SettingsFile::Default, true),
            );
        });
    }

    #[gpui::test]
    async fn acp_beta_creates_existing_string_setting_without_other_flags(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(paths::settings_file().parent().expect("settings parent"))
            .await
            .expect("create fake settings directory");
        fs.insert_file(paths::settings_file(), b"{}".to_vec()).await;
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            <dyn Fs>::set_global(fs.clone(), cx);
            FeatureFlagStore::init(cx);
            set_acp_beta_enabled(true, cx);
        });
        cx.run_until_parked();
        let content = fs
            .load(paths::settings_file())
            .await
            .expect("read persisted settings");
        let content: serde_json::Value =
            serde_json::from_str(&content).expect("valid settings JSON");
        assert_eq!(
            content,
            serde_json::json!({"feature_flags":{"acp-beta":"on"}})
        );
    }
}
