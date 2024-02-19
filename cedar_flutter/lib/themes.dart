import 'package:flutter/material.dart';

ThemeData _normalTheme() {
  return ThemeData(
    brightness: Brightness.dark,
    useMaterial3: true,
  );
}

ThemeData _nightVisionTheme() {
  return ThemeData(
      primaryColor: Colors.red,
      textTheme: const TextTheme(
        bodySmall: TextStyle(color: Colors.red),
        bodyMedium: TextStyle(color: Colors.red),
        bodyLarge: TextStyle(color: Colors.red),
      ),
      colorScheme: const ColorScheme.dark(
        background: Color(0xff202020),
        onBackground: Colors.red,
        surface: Color(0xc0303030),
        onSurface: Colors.red,
        primary: Colors.red,
        onPrimary: Color(0xff404040),
        secondary: Colors.red,
        onSecondary: Color(0xff404040),
        tertiary: Color(0xff808080),
      ),
      useMaterial3: true);
}

class ThemeModel extends ChangeNotifier {
  ThemeData currentTheme = _normalTheme();
  void setNormalTheme() {
    currentTheme = _normalTheme();
    notifyListeners();
  }

  void setNightVisionTheme() {
    currentTheme = _nightVisionTheme();
    notifyListeners();
  }
}
