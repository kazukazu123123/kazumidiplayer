#version 330

// Bends a window render texture based on pitch bend.
// The source texture may include extra transparent padding above/below the
// actual window. The window content's vertical UV range is passed via uniforms.

in vec2 fragTexCoord;
in vec4 fragColor;

uniform sampler2D texture0;

uniform float u_bend;               // Normalized pitch bend (-1.0 to 1.0)
uniform float u_bend_range;         // Current pitch bend range in semitones
uniform float u_max_bend_degrees;   // Visual twist at max bend/range (degrees)
uniform float u_win_width;          // Window width in pixels (without padding)
uniform float u_win_height;         // Window height in pixels (without padding)
uniform float u_curve_power;        // Curve emphasis: higher = stronger at edges
uniform float u_window_top_uv;      // Top edge of window content in source UV
uniform float u_window_bottom_uv;   // Bottom edge of window content in source UV

out vec4 finalColor;

void main()
{
    float window_h_uv = u_window_bottom_uv - u_window_top_uv;
    float win_u = (fragTexCoord.y - u_window_top_uv) / window_h_uv;

    // Edge emphasis: 0.0 at window center, 1.0 at left/right edges.
    float edge_t = abs(fragTexCoord.x - 0.5) * 2.0;
    float edge_curve = pow(edge_t, u_curve_power);

    float bend_sign = sign(u_bend);
    float bend_abs = abs(u_bend);

    // Displacement scales linearly with pitch bend range so extreme ranges
    // keep growing instead of wrapping around tan().
    float base_max_dy_uv = tan(radians(u_max_bend_degrees)) * (u_win_width * 0.5) / max(u_win_height, 1.0);
    float max_dy_uv = base_max_dy_uv * bend_abs * (u_bend_range / 12.0);

    // Pitch bend high curves downward, low curves upward.
    float curve = bend_sign * max_dy_uv * edge_curve;

    float win_v = win_u - curve;
    float src_y = u_window_top_uv + win_v * window_h_uv;

    // Anything that maps outside the original window area is padding/empty.
    if (src_y < u_window_top_uv || src_y > u_window_bottom_uv) {
        finalColor = vec4(0.0, 0.0, 0.0, 0.0);
        return;
    }

    finalColor = texture(texture0, vec2(fragTexCoord.x, src_y)) * fragColor;
}
