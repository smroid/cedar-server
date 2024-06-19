import 'dart:math' as math;
import 'dart:math';
import 'package:flutter/material.dart';
import 'package:sprintf/sprintf.dart';

// angleRad is counter-clockwise starting from up direction, where y increases
// downward. The angle typically corresponds to north (equatorial mount) or
// zenith (alt-az mount).
void drawCross(Canvas canvas, Color color, Offset center, double radius,
    double angleRad, double thickness, double directionThickness) {
  var unitVec = Offset.fromDirection(angleRad + math.pi / 2);
  var unitVecRightAngle = Offset.fromDirection(angleRad);

  canvas.drawLine(
      center.translate(0, 0),
      center.translate(radius * unitVec.dx, -radius * unitVec.dy),
      Paint()
        ..color = color
        ..strokeWidth = directionThickness);
  canvas.drawLine(
      center.translate(0, 0),
      center.translate(-radius * unitVec.dx, radius * unitVec.dy),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(
          -radius * unitVecRightAngle.dx, radius * unitVecRightAngle.dy),
      center.translate(
          radius * unitVecRightAngle.dx, -radius * unitVecRightAngle.dy),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
}

// angleRad is counter-clockwise starting from up direction, where y increases
// downward. The angle typically corresponds to north (equatorial mount) or
// zenith (alt-az mount).
void drawGapCross(
    Canvas canvas,
    Color color,
    Offset center,
    double radius,
    double gapRadius,
    double angleRad,
    double thickness,
    double directionThickness) {
  var unitVec = Offset.fromDirection(angleRad + math.pi / 2);
  var unitVecRightAngle = Offset.fromDirection(angleRad);

  canvas.drawLine(
      center.translate(gapRadius * unitVec.dx, -gapRadius * unitVec.dy),
      center.translate(radius * unitVec.dx, -radius * unitVec.dy),
      Paint()
        ..color = color
        ..strokeWidth = directionThickness);
  canvas.drawLine(
      center.translate(-gapRadius * unitVec.dx, gapRadius * unitVec.dy),
      center.translate(-radius * unitVec.dx, radius * unitVec.dy),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(
          gapRadius * unitVecRightAngle.dx, -gapRadius * unitVecRightAngle.dy),
      center.translate(
          radius * unitVecRightAngle.dx, -radius * unitVecRightAngle.dy),
      Paint()
        ..color = color
        ..strokeWidth = thickness);
  canvas.drawLine(
      center.translate(
          -gapRadius * unitVecRightAngle.dx, gapRadius * unitVecRightAngle.dy),
      center.translate(
          -radius * unitVecRightAngle.dx, radius * unitVecRightAngle.dy),
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

// angleRad is counter-clockwise starting from up direction, where y increases
// downward.
void drawArrow(Canvas canvas, Color color, Offset start, double length,
    double angleRad, String text, double thickness) {
  angleRad +=
      math.pi / 2; // The math below wants angle to start from +x direction.
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

void drawSlewDirections(
  Canvas canvas,
  Color color,
  Offset pos,
  bool altAz, // false: eq
  bool northernHemisphere,
  double offsetRotationAxis, // degrees, az or ra movement
  double offsetTiltAxis, // degrees, alt or dec movement
) {
  final String rotationAxisName = altAz ? "Az " : "RA ";
  final String rotationCue = altAz
      ? (offsetRotationAxis >= 0 ? "clockwise" : "anti-clockwise")
      : (offsetRotationAxis >= 0 ? "towards east" : "towards west");
  final bool towardsPole =
      northernHemisphere ? offsetTiltAxis >= 0 : offsetTiltAxis <= 0;
  final String tiltCue = altAz
      ? (offsetTiltAxis > 0 ? "up" : "down")
      : (towardsPole ? "towards pole" : "away from pole");
  final String tiltAxisName = altAz ? "Alt" : "Dec";
  int precision = 0;
  if (offsetRotationAxis.abs() < 10.0 && offsetTiltAxis.abs() < 10.0) {
    precision = 1;
  }
  if (offsetRotationAxis.abs() < 1.0 && offsetTiltAxis.abs() < 1.0) {
    precision = 2;
  }
  String rotationFormatted = sprintf("%+.*f", [precision, offsetRotationAxis]);
  String tiltFormatted = sprintf("%+.*f", [precision, offsetTiltAxis]);
  final width = max(rotationFormatted.length, tiltFormatted.length);
  // Pad.
  while (rotationFormatted.length < width) {
    rotationFormatted = " $rotationFormatted";
  }
  while (tiltFormatted.length < width) {
    tiltFormatted = " $tiltFormatted";
  }
  const smallFont = 20.0;
  const largeFont = 40.0;
  final textPainter1 = TextPainter(
      text: TextSpan(children: [
        TextSpan(
          text: sprintf("%s ", [rotationAxisName]),
          style: const TextStyle(fontSize: smallFont),
        ),
        TextSpan(
          text: rotationFormatted,
          style:
              const TextStyle(fontSize: largeFont, fontWeight: FontWeight.bold),
        ),
        const TextSpan(text: "°", style: TextStyle(fontSize: largeFont)),
        const TextSpan(text: "\n"),
        TextSpan(
          text: sprintf("%s", [rotationCue]),
          style:
              const TextStyle(fontSize: smallFont, fontStyle: FontStyle.italic),
        ),
        const TextSpan(text: "\n"),
        TextSpan(
          text: sprintf("%s ", [tiltAxisName]),
          style: const TextStyle(fontSize: smallFont),
        ),
        TextSpan(
          text: tiltFormatted,
          style:
              const TextStyle(fontSize: largeFont, fontWeight: FontWeight.bold),
        ),
        const TextSpan(text: "°", style: TextStyle(fontSize: largeFont)),
        const TextSpan(text: "\n"),
        TextSpan(
          text: sprintf("%s", [tiltCue]),
          style:
              const TextStyle(fontSize: smallFont, fontStyle: FontStyle.italic),
        ),
      ], style: TextStyle(fontFamily: "RobotoMono", color: color)),
      textDirection: TextDirection.ltr,
      textAlign: TextAlign.left);
  textPainter1.layout();
  textPainter1.paint(canvas, pos);
}
