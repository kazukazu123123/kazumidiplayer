#version 330

// Input vertex attributes (from Raylib)
in vec2 fragTexCoord;
in vec4 fragColor;

// Input uniform values
uniform sampler2D texture0;         // Current texture (layout_texture or note_texture)
uniform sampler2D hit_data_texture; // Note hit data texture (for hit detection)

uniform float playback_y;           // Normalized Y position in hit_data_texture
uniform float is_texture_valid;     // 1.0 if hit_data_texture is ready
uniform float u_kb_mode;            // 1.0 if drawing keyboard, 0.0 for pass-through

// Output fragment color
out vec4 finalColor;

void main()
{
    // Pass-through mode: Used for piano roll notes and other UI elements
    if (u_kb_mode < 0.5) {
        finalColor = texture(texture0, fragTexCoord) * fragColor;
        return;
    }

    // --- Keyboard Rendering Mode ---
    // Sample the layout mapping (R: note, G: type, B: local_x, A: mask)
    vec4 kb_map = texture(texture0, fragTexCoord);
    
    // If alpha is near 0, it's a border or empty space
    if (kb_map.a < 0.5) {
        finalColor = vec4(0.05, 0.05, 0.05, 1.0);
        return;
    }

    int note_idx = int(kb_map.r * 255.0 / 2.0 + 0.5);
    bool is_white = kb_map.g > 0.7; // White keys have G=1.0 (255), Black keys have G=0.5 (128)
    float local_x = kb_map.b;
    
    bool is_hit = false;
    vec4 note_hit_color = vec4(0.0);
    
    if (is_texture_valid > 0.5) {
        ivec2 tex_size = textureSize(hit_data_texture, 0);
        int py = clamp(int(playback_y * float(tex_size.y - 1)), 0, tex_size.y - 1);
        
        // Point sample the hit texture
        vec4 hit_sample = texelFetch(hit_data_texture, ivec2(note_idx, py), 0);
        
        if (hit_sample.a > 0.1) {
            is_hit = true;
            note_hit_color = hit_sample;
        }
    }

    vec3 res_color;
    if (is_white) {
        float grad = 1.0 - (fragTexCoord.y * 0.1);
        float curve = 1.0 - pow(abs(local_x * 2.0 - 1.0), 4.0) * 0.06;
        res_color = vec3(grad * curve);
    } else {
        float grad = 0.1 + (fragTexCoord.y * 0.2);
        float highlight = smoothstep(0.0, 0.05, fragTexCoord.y) * (1.0 - smoothstep(0.05, 0.15, fragTexCoord.y));
        res_color = vec3(grad) + highlight * 0.1;
        float side = 1.0 - pow(abs(local_x * 2.0 - 1.0), 8.0);
        res_color *= (0.7 + 0.3 * side);
    }

    if (is_hit) {
        vec3 active_color = mix(res_color, note_hit_color.rgb, 0.7);
        float glow = exp(-fragTexCoord.y * 15.0) * 0.8;
        float press = smoothstep(0.1, 1.0, fragTexCoord.y) * 0.2;
        finalColor = vec4(active_color + glow * note_hit_color.rgb - press, 1.0);
    } else {
        finalColor = vec4(res_color, 1.0);
    }
    
    // Shadow at the bottom
    float shadow = smoothstep(0.8, 1.0, fragTexCoord.y);
    finalColor.rgb *= (1.0 - shadow * 0.5);
}
