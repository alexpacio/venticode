FROM maven:3.9-eclipse-temurin-21 AS build
WORKDIR /build
COPY pom.xml ./
RUN --mount=type=cache,target=/root/.m2 mvn -B dependency:go-offline
COPY src src
RUN --mount=type=cache,target=/root/.m2 mvn -B package dependency:copy-dependencies

FROM eclipse-temurin:21-jre
COPY --from=build /build/target/classes /app/classes
COPY --from=build /build/target/dependency /app/lib
USER 65532:65532
ENTRYPOINT ["java","-XX:MaxRAMPercentage=75.0","-cp","/app/classes:/app/lib/*","io.ventic.venticode.Connector"]
