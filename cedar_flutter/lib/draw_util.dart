import 'dart:math' as math;
import 'package:flutter/material.dart';

void drawCross(Canvas canvas, Color color, Offset center, double radius,
    double thickness) {
  canvas.drawLine(
      center.translate(-radius, 0),
      center.translate(radius, 0),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, -radius),
      center.translate(0, radius),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
}

void drawGapCross(Canvas canvas, Color color, Offset center, double radius,
    double gapRadius, double thickness) {
  canvas.drawLine(
      center.translate(-radius, 0),
      center.translate(-gapRadius, 0),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(gapRadius, 0),
      center.translate(radius, 0),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, -radius),
      center.translate(0, -gapRadius),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(0, gapRadius),
      center.translate(0, radius),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
}

void drawText(Canvas canvas, Color color, Offset pos, String text) {
  final textPainter = TextPainter(
      text: TextSpan(text: text, style: TextStyle(color: color, fontSize: 14)),
      textDirection: TextDirection.ltr,
      textAlign: TextAlign.center);
  textPainter.layout();
  textPainter.paint(canvas, pos);
}

void drawArrow(Canvas canvas, Color color, Offset start, double length,
    double angleRad, String text, double thickness) {
  var end = Offset(start.dx + length * math.cos(angleRad),
      start.dy - length * math.sin(angleRad));

  // Adapted from https://stackoverflow.com/questions/72714333
  // (flutter-how-do-i-make-arrow-lines-with-canvas).
  final paint = Paint()
    ..color = color
    ..strokeWidth = thickness;
  canvas.drawLine(start, end, paint);
  const arrowSize = 12;
  const arrowAngle = 25 * math.pi / 180;

  final path = Path();
  path.moveTo(end.dx - arrowSize * math.cos(angleRad - arrowAngle),
      end.dy + arrowSize * math.sin(angleRad - arrowAngle));
  path.lineTo(end.dx, end.dy);
  path.lineTo(end.dx - arrowSize * math.cos(angleRad + arrowAngle),
      end.dy + arrowSize * math.sin(angleRad + arrowAngle));
  path.close();
  canvas.drawPath(path, paint);
  if (text.isNotEmpty) {
    var textPos = Offset(start.dx + (length + 20) * math.cos(angleRad) - 10,
        start.dy - (length + 20) * math.sin(angleRad) - 10);
    drawText(canvas, color, textPos, text);
  }
}
