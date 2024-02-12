import 'package:flutter/material.dart';

void drawCross(Canvas canvas, Offset center, double radius, double thickness) {
  canvas.drawLine(
      center.translate(-radius, 0),
      center.translate(radius, 0),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, -radius),
      center.translate(0, radius),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
}

void drawGapCross(Canvas canvas, Offset center, double radius, double gapRadius,
    double thickness) {
  canvas.drawLine(
      center.translate(-radius, 0),
      center.translate(-gapRadius, 0),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(gapRadius, 0),
      center.translate(radius, 0),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, -radius),
      center.translate(0, -gapRadius),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, gapRadius),
      center.translate(0, radius),
      Paint()
        ..color = Colors.red
        ..strokeWidth = thickness);
}
