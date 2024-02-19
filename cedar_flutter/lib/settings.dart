import 'package:cedar_flutter/cedar.pb.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:settings_ui/settings_ui.dart';
import 'package:sprintf/sprintf.dart';

// Determines if 'prev' and 'curr' have any different fields. Fields that
// are the same are cleared from 'curr'.
bool diffPreferences(Preferences prev, Preferences curr) {
  bool hasDiff = false;
  if (curr.celestialCoordFormat != prev.celestialCoordFormat) {
    hasDiff = true;
  } else {
    curr.clearCelestialCoordFormat();
  }
  if (curr.slewBullseyeSize != prev.slewBullseyeSize) {
    hasDiff = true;
  } else {
    curr.clearSlewBullseyeSize();
  }
  if (curr.nightVisionTheme != prev.nightVisionTheme) {
    hasDiff = true;
  } else {
    curr.clearNightVisionTheme();
  }
  if (curr.showPerfStats != prev.showPerfStats) {
    hasDiff = true;
  } else {
    curr.clearShowPerfStats();
  }
  if (curr.hideAppBar != prev.hideAppBar) {
    hasDiff = true;
  } else {
    curr.clearHideAppBar();
  }
  return hasDiff;
}

class SettingsModel extends ChangeNotifier {
  Preferences preferencesProto = Preferences();

  SettingsModel() {
    preferencesProto.slewBullseyeSize = 1.0;
  }

  void updateCelestialCoordFormat(CelestialCoordFormat format) {
    preferencesProto.celestialCoordFormat = format;
    notifyListeners();
  }

  void updateSlewBullseyeSize(double size) {
    preferencesProto.slewBullseyeSize = size;
    notifyListeners();
  }

  void updateNightVisionEnabled(bool enabled) {
    preferencesProto.nightVisionTheme = enabled;
    notifyListeners();
  }

  void updateShowPerfStats(bool enabled) {
    preferencesProto.showPerfStats = enabled;
    notifyListeners();
  }

  void updateHideAppBar(bool hide) {
    preferencesProto.hideAppBar = hide;
    notifyListeners();
  }
}

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({super.key});

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  @override
  Widget build(BuildContext context) {
    final provider = Provider.of<SettingsModel>(context, listen: false);
    final prefsProto = provider.preferencesProto;
    return Scaffold(
        appBar: AppBar(title: const Text('Preferences')),
        body: SettingsList(
            darkTheme: prefsProto.nightVisionTheme
                ? const SettingsThemeData(
                    titleTextColor: Colors.red,
                    settingsTileTextColor: Colors.red,
                    leadingIconsColor: Colors.red)
                : const SettingsThemeData(),
            sections: [
              SettingsSection(title: const Text('Display'), tiles: [
                // settings_ui has a bug on Web where the 'trailing' element
                // is not visible. We work around this by putting the important
                // element (the control) in the 'leading' position.
                SettingsTile(
                    leading: Switch(
                        value: prefsProto.hideAppBar,
                        onChanged: (bool value) {
                          setState(() {
                            provider.updateHideAppBar(value);
                          });
                        }),
                    title: const Text('Hide app bar'),
                    trailing:
                        const Icon(Icons.close_fullscreen, color: Colors.red)),
                SettingsTile(
                  leading: Switch(
                      value: prefsProto.celestialCoordFormat ==
                          CelestialCoordFormat.HMS_DMS,
                      onChanged: (bool value) {
                        setState(() {
                          provider.updateCelestialCoordFormat(value
                              ? CelestialCoordFormat.HMS_DMS
                              : CelestialCoordFormat.DECIMAL);
                        });
                      }),
                  title: Text(prefsProto.celestialCoordFormat ==
                          CelestialCoordFormat.HMS_DMS
                      ? 'RA/Dec format H.M.S/D.M.S'
                      : 'RA/Dec format D.DD/D.DD'),
                  trailing: const Icon(Icons.numbers),
                ),
                SettingsTile(
                  leading: Switch(
                      value: prefsProto.nightVisionTheme,
                      onChanged: (bool value) {
                        setState(() {
                          provider.updateNightVisionEnabled(value);
                        });
                      }),
                  title: const Text('Night vision theme'),
                  trailing: const Icon(Icons.visibility, color: Colors.red),
                ),
                SettingsTile(
                  leading: Switch(
                      value: prefsProto.showPerfStats,
                      onChanged: (bool value) {
                        setState(() {
                          provider.updateShowPerfStats(value);
                        });
                      }),
                  title: const Text('Show performance stats'),
                  trailing: const Icon(Icons.timer),
                ),
                SettingsTile(
                  leading: Slider(
                    min: 0.1,
                    max: 2.0,
                    divisions: 19,
                    value: prefsProto.slewBullseyeSize,
                    onChanged: (double value) {
                      setState(() {
                        provider.updateSlewBullseyeSize(value);
                      });
                    },
                  ),
                  title: Text(sprintf(
                      'Telescope FOV  %.1f°', [prefsProto.slewBullseyeSize])),
                  trailing: const Icon(Icons.circle_outlined),
                ),
              ])
            ]));
  }
}
