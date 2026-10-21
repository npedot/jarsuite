package dev.jarsuite.examples.commons;

import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Properties;

/**
 * A text file shared by every tool of the suite: {@code <data>/shared.txt}.
 * The first line is {@code format=N}. A store refuses files written in a newer format
 * than its own and upgrades older ones on write, like a real schema migration.
 */
public final class SharedStore {
    public static final String VERSION;
    public static final int FORMAT;

    static {
        Properties p = new Properties();
        try (InputStream in = SharedStore.class.getResourceAsStream("commons.properties")) {
            p.load(in);
        } catch (IOException e) {
            throw new UncheckedIOException(e);
        }
        VERSION = p.getProperty("version");
        FORMAT = Integer.parseInt(p.getProperty("format"));
    }

    private final Path file;

    public SharedStore(Path dir) {
        this.file = dir.resolve("shared.txt");
    }

    /** {@code JARSUITE_DATA} if set, otherwise {@code data/} in the current directory. */
    public static SharedStore open() {
        String env = System.getenv("JARSUITE_DATA");
        return new SharedStore(Path.of(env != null && !env.isEmpty() ? env : "data"));
    }

    public Path file() {
        return file;
    }

    public List<String> read() throws IOException {
        if (!Files.exists(file)) {
            return List.of();
        }
        List<String> lines = Files.readAllLines(file, StandardCharsets.UTF_8);
        checkFormat(lines);
        return lines.subList(1, lines.size());
    }

    public void append(String entry) throws IOException {
        List<String> entries = new ArrayList<>(read());
        entries.add(entry);
        List<String> out = new ArrayList<>();
        out.add("format=" + FORMAT);
        out.addAll(entries);
        Files.createDirectories(file.getParent() == null ? Path.of(".") : file.getParent());
        Files.write(file, out, StandardCharsets.UTF_8);
    }

    private void checkFormat(List<String> lines) {
        if (lines.isEmpty() || !lines.get(0).startsWith("format=")) {
            throw new DataFormatException(file + ": missing format header");
        }
        int found = Integer.parseInt(lines.get(0).substring("format=".length()).trim());
        if (found > FORMAT) {
            throw new DataFormatException(file + ": data format " + found
                    + " is newer than supported " + FORMAT + " (commons " + VERSION + ")");
        }
    }

    public static final class DataFormatException extends RuntimeException {
        public DataFormatException(String message) {
            super(message);
        }
    }
}
