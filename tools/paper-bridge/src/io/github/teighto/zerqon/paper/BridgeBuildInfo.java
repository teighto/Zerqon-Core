package io.github.teighto.zerqon.paper;

import io.papermc.paper.ServerBuildInfo;
import java.time.Instant;
import java.io.IOException;
import java.util.Properties;
import java.util.Optional;
import java.util.OptionalInt;
import net.kyori.adventure.key.Key;

public final class BridgeBuildInfo implements ServerBuildInfo {
    private final Instant builtAt;
    public BridgeBuildInfo() {
        Properties metadata = new Properties();
        try (var resource = BridgeBuildInfo.class.getResourceAsStream("/zerqon-paper-build.properties")) {
            if (resource == null) { throw new IllegalStateException("Missing bridge build metadata"); }
            metadata.load(resource);
            builtAt = Instant.parse(metadata.getProperty("build-time"));
        } catch (IOException failure) { throw new IllegalStateException("Cannot read bridge build metadata", failure); }
    }
    @Override public Key brandId() { return Key.key("teighto", "zerqon"); }
    @Override public boolean isBrandCompatible(Key brand) { return brandId().equals(brand); }
    @Override public String brandName() { return "Zerqon"; }
    @Override public String minecraftVersionId() { return "26.3"; }
    @Override public String minecraftVersionName() { return "26.3"; }
    @Override public OptionalInt buildNumber() { return OptionalInt.empty(); }
    @Override public Instant buildTime() { return builtAt; }
    @Override public Optional<String> gitBranch() { return Optional.empty(); }
    @Override public Optional<String> gitCommit() { return Optional.empty(); }
    @Override public String asString(StringRepresentation representation) { return "0.0.1-experimental-paper-bridge"; }
}
